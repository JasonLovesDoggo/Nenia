//! Detached workers must not keep the hook parent's inherited pipes alive.
use anyhow::{ensure, Context, Result};
use std::{ffi::OsStr, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::Threading::{
        CreateProcessW, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    },
};

fn quoted(arg: &OsStr) -> Result<Vec<u16>> {
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for unit in arg.encode_wide() {
        ensure!(unit != 0, "worker argument contains NUL");
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let escaped = if unit == b'"' as u16 {
            slashes * 2 + 1
        } else {
            slashes
        };
        result.extend(std::iter::repeat_n(b'\\' as u16, escaped));
        slashes = 0;
        result.push(unit);
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    Ok(result)
}

pub(crate) fn spawn(executable: &Path, vault: &Path) -> Result<()> {
    let mut application: Vec<_> = executable.as_os_str().encode_wide().collect();
    ensure!(!application.contains(&0), "worker executable contains NUL");
    application.push(0);
    let mut command = Vec::new();
    for arg in [
        executable.as_os_str(),
        OsStr::new("--vault"),
        vault.as_os_str(),
        OsStr::new("hooks"),
        OsStr::new("drain"),
    ] {
        if !command.is_empty() {
            command.push(b' ' as u16);
        }
        command.extend(quoted(arg)?);
    }
    command.push(0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: all strings are NUL-terminated, the writable command buffer lives
    // through the call, and output structures have their required size/layout.
    // FALSE is essential: Stdio::null alone does not stop Rust 1.88's Command
    // from inheriting other open pipe handles from the hook's caller.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            ptr::null(),
            ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error()).context("start detached Windows worker");
    }
    // SAFETY: successful CreateProcessW transfers these two owned handles to us.
    // Closing our references does not terminate the child process.
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arguments_preserve_literal_paths_and_trailing_slashes() {
        assert_eq!(
            String::from_utf16(&quoted(OsStr::new("C:\\path with spaces\\")).unwrap()).unwrap(),
            "\"C:\\path with spaces\\\\\""
        );
        assert_eq!(
            String::from_utf16(&quoted(OsStr::new("a\"b")).unwrap()).unwrap(),
            "\"a\\\"b\""
        );
        assert!(quoted(OsStr::new("bad\0path")).is_err());
    }
}
