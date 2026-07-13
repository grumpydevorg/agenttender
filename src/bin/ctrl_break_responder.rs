//! Test fixture: exits with code 42 when it receives CTRL_BREAK.
//!
//! Used by tests/windows_child.rs to verify that graceful kill via
//! GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT) works without escalating
//! to TerminateJobObject.

#[cfg(windows)]
fn main() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static GOT_BREAK: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        const CTRL_BREAK_EVENT: u32 = 1;
        if ctrl_type == CTRL_BREAK_EVENT {
            GOT_BREAK.store(true, Ordering::Relaxed);
            return 1; // TRUE — handled, don't terminate
        }
        0 // FALSE — let default handler run
    }

    // SAFETY: handler is a valid extern "system" function with the
    // correct signature for SetConsoleCtrlHandler. We pass TRUE (1)
    // to add (not remove) the handler.
    let installed =
        unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(handler), 1) };

    // READY must *prove* installation, not merely follow the attempt. Bail loudly
    // if the handler was not installed, so the marker can never be published by a
    // process that would instead be killed by the default CTRL_BREAK handler.
    if installed == 0 {
        eprintln!(
            "SetConsoleCtrlHandler failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(1);
    }

    // Readiness handshake: the handler is now proven installed, so any CTRL_BREAK
    // from this point on is caught. The test waits for this line before signalling,
    // which replaces a fixed "give it a moment to install" sleep. Flushed so the
    // line is observable immediately rather than sitting in a buffer.
    {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = writeln!(out, "READY");
        let _ = out.flush();
    }

    // Poll until CTRL_BREAK is received.
    while !GOT_BREAK.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    std::process::exit(42);
}

#[cfg(not(windows))]
fn main() {
    // Stub for non-Windows builds.
}
