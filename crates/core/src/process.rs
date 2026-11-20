//! Processes that are to outlive the app.
//!
//! On Windows the desktop app puts itself in a job that ends every process
//! it started when it ends, crashed or not (`platform::tie_children`): an
//! agent's process and what that runs never carry on alone. What the app
//! starts for the user (a link's browser, a terminal to sign in, the next
//! version after an update) is theirs and is started here instead, outside
//! that job.

use std::io;
use std::process::{Child, Command};

/// Starts `command` so that it keeps running after the app ends, with the
/// Windows creation `flags` it needs besides (ignored elsewhere, where a
/// child outlives its parent anyway).
///
/// # Errors
/// The program could not be started.
pub fn spawn_lasting(command: &mut Command, flags: u32) -> io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
        match command.spawn() {
            // A job the app runs in (a terminal's, say) does not let it go: started as it is.
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                command.creation_flags(flags);
                command.spawn()
            }
            started => started,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = flags;
        command.spawn()
    }
}
