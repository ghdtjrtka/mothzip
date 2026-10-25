//! Human-terminal password input. Refuse input if console echo cannot be disabled.
use std::io::{self, Write};

#[cfg(windows)]
pub fn read_password(archive: &str) -> io::Result<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(kind: u32) -> *mut std::ffi::c_void;
        fn GetConsoleMode(handle: *mut std::ffi::c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut std::ffi::c_void, mode: u32) -> i32;
    }
    struct RestoreEcho {
        handle: *mut std::ffi::c_void,
        mode: u32,
    }
    impl Drop for RestoreEcho {
        fn drop(&mut self) {
            unsafe { SetConsoleMode(self.handle, self.mode) };
        }
    }
    // SAFETY: these calls only inspect/change the process's own console input mode.
    let handle = unsafe { GetStdHandle(-10i32 as u32) };
    let mut mode = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    const ENABLE_ECHO_INPUT: u32 = 0x0004;
    if unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let guard = RestoreEcho { handle, mode };
    eprint!("Enter password for '{}': ", archive);
    io::stderr().flush()?;
    let mut value = String::new();
    let result = io::stdin().read_line(&mut value);
    drop(guard);
    eprintln!();
    result?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(not(windows))]
pub fn read_password(_archive: &str) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Secure password prompting requires a Windows console",
    ))
}
