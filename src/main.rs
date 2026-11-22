mod backend;
mod config_tui;
mod daemon;
mod diag;
mod gpu;
mod guide;
mod input;
mod note;
mod perf;
mod render;
#[cfg(test)]
mod sim_tests;
mod watchdog;
mod win32;
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::PathBuf,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::{Pipes::*, Threading::*},
    UI::HiDpi::*,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
/// Optional isolation suffix so test daemons never share the pipe or mutex of a real session.
fn instance() -> String {
    std::env::var("PLANOR_INSTANCE")
        .map(|v| format!("-{v}"))
        .unwrap_or_default()
}
/// Test mode disables OS-wide global hotkeys, raw input registration, low-level hooks, and
/// interactive Win+W note popups so isolated test daemons can coexist with the user's real daemon.
pub(crate) fn test_mode() -> bool {
    if let Ok(v) = std::env::var("PLANOR_TEST_MODE") {
        if v == "1" || v.eq_ignore_ascii_case("true") {
            return true;
        }
        if v == "0" || v.eq_ignore_ascii_case("false") {
            return false;
        }
    }
    !instance().is_empty()
}
fn pipe_name() -> String {
    format!(r"\\.\pipe\planor-ipc{}", instance())
}
const EXAMPLE: &str = include_str!("../planor.example.toml");
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
fn folder(var: &str) -> Result<PathBuf> {
    let dir =
        PathBuf::from(std::env::var_os(var).ok_or("missing profile directory")?).join("planor");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}
fn config() -> Result<toml::Value> {
    let path = folder("APPDATA")?.join("planor.toml");
    match File::create_new(&path) {
        Ok(mut f) => f.write_all(EXAMPLE.as_bytes())?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    };
    parse(&fs::read_to_string(path)?)
}
/// Choices for the settings that take one of a few words (also offered by `planor config`).
pub(crate) fn choices(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "curve" => Some(&["ease_out_cubic", "spring"]),
        "drag_stack_modifier" => Some(&["Shift", "Ctrl", "Alt"]),
        _ => None,
    }
}
/// Whether `v` may replace the default `old` for setting `k`: same type, sane range, a known word, a
/// `#RRGGBB` colour. Shared by the daemon's config merge and `planor config`.
pub(crate) fn valid_value(k: &str, old: &toml::Value, v: &toml::Value) -> bool {
    let typed = old.type_str() == v.type_str()
        && v.as_integer().is_none_or(|n| (0..=100000).contains(&n))
        && v.as_float().is_none_or(|n| n.is_finite() && n > 0. && (k != "default_tile_share" || n <= 1.));
    let known = match (choices(k), k) {
        (Some(c), _) => v.as_str().is_some_and(|s| c.contains(&s)),
        (None, k) if k.ends_with("_color") => v
            .as_str()
            .is_some_and(|v| v.len() == 7 && v.starts_with('#') && v[1..].bytes().all(|b| b.is_ascii_hexdigit())),
        _ => true,
    };
    typed && known
}
fn parse(s: &str) -> Result<toml::Value> {
    let mut cfg = toml::from_str::<toml::Value>(EXAMPLE)?;
    let incoming = toml::from_str::<toml::Table>(s)?;
    for (section, value) in incoming {
        if section == "rules" {
            if let Some(rows) = value.as_array() {
                cfg["rules"] = toml::Value::Array(
                    rows.iter()
                        .filter(|r| {
                            matches!(
                                r.get("action").and_then(|v| v.as_str()),
                                Some("ignore" | "float" | "tile")
                            ) && ["process", "class"]
                                .iter()
                                .any(|k| r.get(*k).and_then(|v| v.as_str()).is_some())
                        })
                        .cloned()
                        .collect(),
                )
            }
            continue;
        }
        let Some(base) = cfg.get_mut(&section).and_then(|v| v.as_table_mut()) else {
            eprintln!("unknown config section");
            continue;
        };
        if let Some(table) = value.as_table() {
            for (k, v) in table {
                if let Some(old) = base.get_mut(k) {
                    if valid_value(k, old, v) {
                        *old = v.clone();
                        continue;
                    }
                }
                eprintln!("ignored invalid config key in {section}")
            }
        }
    }
    Ok(cfg)
}
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
enum Command {
    Start,
    Stop,
    Reload,
    Pause,
    Resume,
    Pin,
    PullOrphans,
    /// Trigger a hotkey action over IPC (for isolated test mode without global hotkeys).
    Key {
        name: String,
    },
    /// Dump current monitors, strips, tiles, parked/pinned/orphan windows.
    State {
        #[arg(long)]
        json: bool,
    },
    /// Inspect timing spans, process/thread telemetry, and budgets, or export a QPC Chrome trace.
    Perf {
        #[arg(long)]
        reset: bool,
        #[arg(long)]
        trace: Option<PathBuf>,
        #[arg(long)]
        seconds: Option<u64>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    Autostart {
        mode: Switch,
    },
    Diag {
        #[command(subcommand)]
        action: Diagnostic,
    },
    /// Record real touchpad gestures into a fixture file the tests replay (landscape and portrait).
    Gestures {
        #[command(subcommand)]
        action: GestureAction,
    },
    /// Edit settings (%APPDATA%\planor\planor.toml) in a terminal UI.
    Config,
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
enum GestureAction {
    /// Capture every touchpad frame the daemon sees for `seconds`, then write `out` (events.jsonl format).
    Record {
        /// Short name, e.g. "scroll-right-slow"; used in the file name.
        name: String,
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Output file (default: %LOCALAPPDATA%\planor\gestures\<stamp>-<name>.jsonl).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Record only: Planor does not act on the gestures while recording.
        #[arg(long)]
        passive: bool,
    },
    /// Guided session: a window on top walks through every gesture (1-5 fingers, taps, clicks, all
    /// directions) while the daemon records them, marked step by step.
    Guide {
        #[arg(default_value = "guided")]
        name: String,
        /// Seconds per step (plus 2 s to get ready).
        #[arg(long, default_value_t = 6)]
        step_seconds: u64,
        /// Let Planor act on the gestures while recording (default: record only).
        #[arg(long)]
        live: bool,
    },
    /// Add a step label to the running recording.
    Mark { text: String },
    /// Finish the running recording now and write it.
    Stop,
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
enum Diagnostic {
    On,
    Off,
    Live,
    Export { path: PathBuf },
}
#[derive(ValueEnum, Clone, Serialize, Deserialize, Debug)]
enum Switch {
    On,
    Off,
}
fn identity_acl() -> Result<*mut std::ffi::c_void> {
    unsafe {
        let mut handle = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let _token = File::from_raw_handle(handle);
        let mut size = 0;
        GetTokenInformation(handle, TokenUser, null_mut(), 0, &mut size);
        let mut buf = vec![0usize; (size as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(handle, TokenUser, buf.as_mut_ptr().cast(), size, &mut size) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut ptr = null_mut();
        if ConvertSidToStringSidW((*(buf.as_ptr() as *const TOKEN_USER)).User.Sid, &mut ptr) == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        LocalFree(ptr.cast());
        let mut sd = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(&format!("D:P(A;;GRGW;;;{sid})S:(ML;;NW;;;ME)")).as_ptr(),
            1,
            &mut sd,
            null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(sd)
    }
}
fn pipe(name: &str) -> Result<File> {
    unsafe {
        let sd = identity_acl()?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as _,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let h = CreateNamedPipeW(
            wide(name).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            4096,
            0,
            &sa,
        );
        let err = io::Error::last_os_error();
        LocalFree(sd);
        if h == INVALID_HANDLE_VALUE {
            return Err(err.into());
        }
        Ok(File::from_raw_handle(h))
    }
}
fn frame(f: &mut File, data: &[u8]) -> Result<()> {
    f.write_all(&(u32::try_from(data.len())?).to_le_bytes())?;
    f.write_all(data)?;
    Ok(())
}
fn read_frame(f: &mut File) -> Result<Vec<u8>> {
    let mut size = [0; 4];
    f.read_exact(&mut size)?;
    let n = u32::from_le_bytes(size);
    if n > 16_777_216 {
        return Err("oversized IPC message".into());
    }
    let mut data = vec![0; n as usize];
    f.read_exact(&mut data)?;
    Ok(data)
}
struct PipeServer {
    file: File,
    connected: bool,
    reply: Vec<u8>,
    sent: usize,
    started: std::time::Instant,
    closing: bool,
}
impl PipeServer {
    fn new(name: &str) -> Result<Self> {
        let file = pipe(name)?;
        let mode = PIPE_NOWAIT;
        if unsafe { SetNamedPipeHandleState(file.as_raw_handle(), &mode, null(), null()) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self {
            file,
            connected: false,
            reply: vec![],
            sent: 0,
            started: std::time::Instant::now(),
            closing: false,
        })
    }
    fn has_pending_write(&self) -> bool {
        self.sent < self.reply.len()
    }
    fn reset(&mut self) {
        unsafe {
            DisconnectNamedPipe(self.file.as_raw_handle());
        }
        self.connected = false;
        self.reply.clear();
        self.sent = 0
    }
    fn poll(&mut self, mut handle: impl FnMut(Command) -> Result<String>) -> Result<bool> {
        let h = self.file.as_raw_handle();
        if !self.connected {
            let ok = unsafe { ConnectNamedPipe(h, null_mut()) };
            if ok == 0 {
                match unsafe { GetLastError() } {
                    ERROR_PIPE_LISTENING => return Ok(false),
                    ERROR_PIPE_CONNECTED => (),
                    ERROR_NO_DATA => {
                        self.reset();
                        return Ok(self.closing);
                    }
                    _ => return Err(io::Error::last_os_error().into()),
                }
            }
            self.connected = true;
            self.started = std::time::Instant::now()
        }
        let mut available = 0;
        let mut prefix = [0; 4];
        let mut n = 0;
        if unsafe {
            PeekNamedPipe(
                h,
                prefix.as_mut_ptr().cast(),
                4,
                &mut n,
                &mut available,
                null_mut(),
            )
        } == 0
            || self.started.elapsed().as_secs() > 10
        {
            self.reset();
            return Ok(self.closing);
        }
        if self.reply.is_empty() && n == 4 {
            let len = u32::from_le_bytes(prefix);
            if len > 65536 {
                self.reset();
                return Ok(false);
            }
            if available >= 4 + len {
                let result = read_frame(&mut self.file)
                    .and_then(|data| Ok(serde_json::from_slice::<Command>(&data)?))
                    .and_then(|cmd| {
                        let stop = matches!(cmd, Command::Stop)
                            || matches!(&cmd, Command::Key { name } if name == "panic_exit");
                        let result = handle(cmd);
                        self.closing = stop && result.is_ok();
                        result
                    })
                    .map_err(|e| e.to_string());
                let body = serde_json::to_vec(&result)?;
                self.reply
                    .extend_from_slice(&(body.len() as u32).to_le_bytes());
                self.reply.extend(body)
            }
        }
        while self.sent < self.reply.len() {
            match self.file.write(&self.reply[self.sent..]) {
                Ok(0) => break,
                Ok(n) => self.sent += n,
                Err(e)
                    if e.raw_os_error() == Some(ERROR_NO_DATA as i32)
                        || e.kind() == io::ErrorKind::WouldBlock =>
                {
                    break;
                }
                Err(e)
                    if matches!(
                        e.raw_os_error().map(|c| c as u32),
                        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
                    ) =>
                {
                    self.reset();
                    return Ok(self.closing);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(false)
    }
}
fn client(name: &str, cmd: &Command) -> Result<String> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut file = loop {
        match OpenOptions::new().read(true).write(true).open(name) {
            Ok(f) => break f,
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && std::time::Instant::now() < until =>
            {
                unsafe {
                    WaitNamedPipeW(wide(name).as_ptr(), 50);
                }
                std::thread::yield_now()
            }
            Err(e) => return Err(e.into()),
        }
    };
    frame(&mut file, &serde_json::to_vec(cmd)?)?;
    let response: std::result::Result<String, String> =
        serde_json::from_slice(&read_frame(&mut file)?)?;
    Ok(response?)
}
fn autostart(mode:Switch)->Result<()>{use windows_sys::Win32::System::Registry::*;let cmd=wide(&format!("\"{}\" start",std::env::current_exe()?.display()));unsafe{let mut key=null_mut();let path=wide(r"Software\Microsoft\Windows\CurrentVersion\Run");let status=RegCreateKeyExW(HKEY_CURRENT_USER,path.as_ptr(),0,null(),REG_OPTION_NON_VOLATILE,KEY_SET_VALUE,null(),&mut key,null_mut());if status!=0{return Err(io::Error::from_raw_os_error(status as i32).into())}let name=wide("Planor");let result=match mode{Switch::On=>RegSetValueExW(key,name.as_ptr(),0,REG_SZ,cmd.as_ptr().cast(),(cmd.len()*2)as u32),Switch::Off=>RegDeleteValueW(key,name.as_ptr())};RegCloseKey(key);if result!=0&&result!=ERROR_FILE_NOT_FOUND{return Err(io::Error::from_raw_os_error(result as i32).into())}Ok(())}}
fn main() -> Result<()> {
    // Panics in any thread are appended to crash.log (the daemon usually runs without a console).
    std::panic::set_hook(Box::new(|info| {
        let line = format!("{info}\n{}\n", std::backtrace::Backtrace::force_capture());
        eprintln!("{line}");
        if let Ok(dir) = folder("LOCALAPPDATA") {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))
                .and_then(|mut f| io::Write::write_all(&mut f, line.as_bytes()));
        }
    }));
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let build = unsafe {
        let mut info = [0u32; 71];
        info[0] = 284;
        unsafe extern "system" {
            fn RtlGetVersion(info: *mut u32) -> i32;
        }
        #[link(name = "ntdll")]
        unsafe extern "system" {}
        if RtlGetVersion(info.as_mut_ptr())<0||info[1]!=10||info[70].to_le_bytes()[2]!=1 {
            0
        } else {
            info[3]
        }
    };
    if build < 26300 {
        return Err("Windows build 26300 or newer required".into());
    }
    let cmd = Cli::parse().cmd.unwrap_or(Command::Start);
    match cmd {
        Command::Start => daemon::run(),
        Command::Autostart{mode}=>autostart(mode),
        Command::Diag {
            action: Diagnostic::Export { ref path },
        } => {
            fs::write(path, client(&pipe_name(), &cmd)?)?;
            Ok(())
        }
        Command::Diag {
            action: Diagnostic::Live,
        } => loop {
            println!("{}", client(&pipe_name(), &cmd)?);
            std::thread::sleep(std::time::Duration::from_secs(1))
        },
        Command::Perf {
            bundle: Some(ref dir),
            seconds,
            ..
        } => {
            fs::create_dir_all(dir)?;
            let state_json = client(&pipe_name(), &Command::State { json: true })?;
            let perf_json = client(
                &pipe_name(),
                &Command::Perf {
                    reset: false,
                    trace: None,
                    seconds,
                    json: true,
                    bundle: None,
                },
            )?;
            let trace_json = client(
                &pipe_name(),
                &Command::Perf {
                    reset: false,
                    trace: Some(PathBuf::from("-")),
                    seconds: seconds.or(Some(15)),
                    json: false,
                    bundle: None,
                },
            )?;
            fs::write(dir.join("state.json"), state_json)?;
            fs::write(dir.join("perf.json"), perf_json)?;
            fs::write(dir.join("perf.trace.json"), trace_json)?;
            if let Ok(local) = folder("LOCALAPPDATA") {
                let trace_log = local.join("trace.log");
                if let Ok(raw) = fs::read_to_string(&trace_log) {
                    let lines: Vec<&str> = raw.lines().collect();
                    let start = lines.len().saturating_sub(400);
                    fs::write(dir.join("trace.tail.log"), lines[start..].join("\n"))?;
                }
            }
            if let Ok(exe) = std::env::current_exe() {
                let candidates = [
                    exe.parent().map(|d| d.join("planor-rec.exe")),
                    Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tools/planor-rec-bin/planor-rec.exe")),
                ];
                if let Some(rec) = candidates.into_iter().flatten().find(|p| p.is_file()) {
                    let _ = std::process::Command::new(rec)
                        .args(["dump", "--out"])
                        .arg(dir.join("video"))
                        .args(["--reason", "perf-bundle", "--pipe", "ring"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
            println!("{}", dir.display());
            Ok(())
        }
        Command::Perf {
            trace: Some(ref path),
            seconds,
            ..
        } => {
            let payload = client(
                &pipe_name(),
                &Command::Perf {
                    reset: false,
                    trace: Some(path.clone()),
                    seconds,
                    json: false,
                    bundle: None,
                },
            )?;
            if path.as_os_str() == "-" {
                println!("{payload}");
            } else {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        fs::create_dir_all(parent)?;
                    }
                }
                fs::write(path, payload)?;
                println!("{}", path.display());
            }
            Ok(())
        }
        Command::Stop => match client(&pipe_name(), &cmd) {
            Ok(reply) => {
                println!("{reply}");
                Ok(())
            }
            Err(_) => {
                // No daemon is listening: it crashed, was killed, or its console was closed. Restore
                // whatever its journal still holds so no window stays parked off-screen.
                let restored = daemon::recover()?;
                println!("no daemon running; restored {restored} window(s) from the recovery journal");
                Ok(())
            }
        },
        Command::Config => config_tui::run(&folder("APPDATA")?.join("planor.toml")),
        Command::Gestures {
            action: GestureAction::Record { name, seconds, out, passive },
        } => record_gestures(name, seconds, out, passive),
        Command::Gestures {
            action: GestureAction::Guide { name, step_seconds, live },
        } => guide::run(name, step_seconds, live),
        _ => {
            println!("{}", client(&pipe_name(), &cmd)?);
            Ok(())
        }
    }
}
/// `planor gestures record <name>`: the daemon captures every touchpad frame it sees for `seconds` and
/// writes them (events.jsonl, plus a meta line) to the output file; this side waits and summarises.
fn record_gestures(name: String, seconds: u64, out: Option<PathBuf>, passive: bool) -> Result<()> {
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let out = match out {
        Some(p) => std::path::absolute(p)?,
        None => {
            let stamp = diag::stamp().replace([':', '.'], "");
            folder("LOCALAPPDATA")?.join("gestures").join(format!("{stamp}-{slug}.jsonl"))
        }
    };
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir)?;
    }
    let _ = fs::remove_file(&out);
    let reply = client(
        &pipe_name(),
        &Command::Gestures { action: GestureAction::Record { name: slug, seconds, out: Some(out.clone()), passive } },
    )?;
    println!("{reply}");
    println!("Recording for {seconds} s: do the gesture(s) now on the touchpad (each one, then lift all fingers).");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < until {
        let left = until.saturating_duration_since(std::time::Instant::now()).as_secs();
        print!("\r  {left:>3} s left ");
        let _ = io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // The daemon writes the file on its next tick.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !out.is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!();
    let text = fs::read_to_string(&out).map_err(|e| format!("no recording written to {}: {e}", out.display()))?;
    let events = planor::replay::parse_events_jsonl(&text);
    let mut gestures = vec![];
    let mut fingers = 0;
    for e in &events {
        if let planor::replay::ReplayKind::Contacts(p) = &e.kind {
            if p.is_empty() {
                if fingers > 0 {
                    gestures.push(fingers)
                }
                fingers = 0;
            } else {
                fingers = fingers.max(p.len());
            }
        }
    }
    if fingers > 0 {
        gestures.push(fingers)
    }
    println!(
        "{} frames, {} gesture(s) ({}) -> {}",
        events.len(),
        gestures.len(),
        gestures.iter().map(|f| format!("{f} fingers")).collect::<Vec<_>>().join(", "),
        out.display()
    );
    println!("Give this path to Claude to add it under tests/fixtures/gestures/recorded/.");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed() {
        assert!(parse("[bad").is_err());
        assert_eq!(
            parse("[layout]\ngap_px=-4").unwrap()["layout"]["gap_px"].as_integer(),
            Some(6)
        )
    }
    #[test]
    fn round_trip() {
        let name = format!(r"\\.\pipe\planor-test-{}", std::process::id());
        let mut server = PipeServer::new(&name).unwrap();
        let worker = std::thread::spawn(move || -> Result<()> {
            while server.reply.is_empty() || server.sent < server.reply.len() {
                server.poll(|c| Ok(format!("{c:?}")))?;
                std::thread::yield_now()
            }
            unsafe {
                FlushFileBuffers(server.file.as_raw_handle());
            }
            Ok(())
        });
        assert_eq!(client(&name, &Command::Pause).unwrap(), "Pause");
        worker.join().unwrap().unwrap()
    }
    #[test]
    fn large_reply_round_trip() {
        let name = format!(r"\\.\pipe\planor-test-large-{}", std::process::id());
        let mut server = PipeServer::new(&name).unwrap();
        let big = "x".repeat(180_000);
        let expected = big.clone();
        let worker = std::thread::spawn(move || -> Result<()> {
            while !server.connected || server.reply.is_empty() || server.sent < server.reply.len() {
                server.poll(|_| Ok(big.clone()))?;
                std::thread::yield_now()
            }
            unsafe {
                FlushFileBuffers(server.file.as_raw_handle());
            }
            Ok(())
        });
        assert_eq!(client(&name, &Command::Pause).unwrap(), expected);
        worker.join().unwrap().unwrap()
    }
}
