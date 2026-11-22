//! Named-pipe control channel: `planor-rec stop` / `planor-rec mark ...` talk to a running recorder.

use crate::events::EventWriter;
use serde_json::json;
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX,
    },
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    },
};

pub const PIPE_NAME: &str = r"\\.\pipe\planor-rec";

/// The pipe a recorder listens on: the default, or `\\.\pipe\planor-rec-<name>` so a test recorder and a
/// manual one can run side by side.
pub fn pipe_path(name: Option<&str>) -> String {
    match name {
        Some(n) if !n.trim().is_empty() => format!(r"\\.\pipe\planor-rec-{}", n.trim()),
        _ => PIPE_NAME.to_string(),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Send one command to a running recorder. Returns its reply ("ok").
pub fn client(command: &str, pipe: Option<&str>) -> io::Result<String> {
    unsafe {
        let handle: HANDLE = CreateFileW(
            wide(&pipe_path(pipe)).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut line = command.trim().to_string();
        line.push('\n');
        let mut written = 0u32;
        let ok = WriteFile(
            handle,
            line.as_ptr(),
            line.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        );
        if ok == 0 {
            CloseHandle(handle);
            return Err(io::Error::last_os_error());
        }
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let mut read = 0u32;
            let ok = ReadFile(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            );
            if ok == 0 || read == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..read as usize]);
        }
        CloseHandle(handle);
        if bytes.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty pipe reply"));
        }
        Ok(String::from_utf8_lossy(&bytes).trim().to_string())
    }
}

fn read_command(handle: HANDLE) -> Option<String> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || read == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..read as usize]);
        if bytes.contains(&b'\n') || bytes.len() > 4096 {
            break;
        }
    }
    if bytes.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes).trim().to_string())
    }
}

fn reply(handle: HANDLE, text: &str) {
    let mut line = text.to_string();
    line.push('\n');
    let mut written = 0u32;
    unsafe {
        WriteFile(
            handle,
            line.as_ptr(),
            line.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        );
    }
}

pub type CommandHandler = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// Serve `stop`, `mark <text>` and `ping` until the recorder stops.
pub fn spawn_server(
    writer: Arc<EventWriter>,
    stop: Arc<AtomicBool>,
    pipe: Option<String>,
) -> thread::JoinHandle<()> {
    spawn_server_with(writer, stop, pipe, None)
}

/// Serve pipe commands plus an optional custom command handler (used by ring capture for `dump` and `status`).
pub fn spawn_server_with(
    writer: Arc<EventWriter>,
    stop: Arc<AtomicBool>,
    pipe: Option<String>,
    handler: Option<CommandHandler>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let name = wide(&pipe_path(pipe.as_deref()));
        while !stop.load(Ordering::Relaxed) {
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    8192,
                    8192,
                    0,
                    std::ptr::null(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return;
            }
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if connected {
                if let Some(command) = read_command(handle) {
                    let mut words = command.splitn(2, char::is_whitespace);
                    let verb = words.next().unwrap_or("");
                    let rest = words.next().unwrap_or("").trim();
                    match verb {
                        "stop" => {
                            reply(handle, "ok: stopping");
                            stop.store(true, Ordering::Relaxed);
                        }
                        "mark" => {
                            let text = if rest.is_empty() {
                                "mark".to_string()
                            } else {
                                rest.to_string()
                            };
                            writer.write(json!({"kind": "mark", "text": text, "source": "pipe"}));
                            reply(handle, "ok: marked");
                        }
                        "ping" => reply(handle, "ok: recording"),
                        other => {
                            if let Some(h) = &handler {
                                if let Some(resp) = h(other, rest) {
                                    reply(handle, &resp);
                                } else {
                                    reply(handle, &format!("error: unknown command `{other}`"));
                                }
                            } else {
                                reply(handle, &format!("error: unknown command `{other}`"));
                            }
                        }
                    }
                }
            }
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
            thread::sleep(Duration::from_millis(5));
        }
        writer.flush_marker();
    })
}

impl EventWriter {
    /// Small helper used when the pipe server shuts down; keeps the stream flushed.
    fn flush_marker(&self) {
        self.write(json!({"kind": "session", "event": "pipe-closed"}));
    }
}

/// Convenience wrapper used by `main`.
pub fn send(command: &str, pipe: Option<&str>) -> io::Result<String> {
    client(command, pipe)
}
