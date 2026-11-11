//! Optional VB-CABLE add-on for full processing mode.
//!
//! VB-CABLE is VB-Audio's product (www.vb-cable.com), distributed as
//! donationware. The installer carries VB-Audio's driver pack unmodified.
//! Nothing is installed silently: the user presses a button, Windows asks for
//! administrator approval, and VB-Audio's own setup window opens.

use std::path::{Path, PathBuf};

const SETUP_EXE: &str = "VBCABLE_Setup_x64.exe";

/// Locate VB-Audio's setup program inside the app's bundled resources.
pub fn bundled_setup(resource_dir: &Path) -> Option<PathBuf> {
    let root = resource_dir.join("vbcable");
    let direct = root.join(SETUP_EXE);
    if direct.is_file() {
        return Some(direct);
    }
    // The pack may unpack into one sub-folder.
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        let candidate = entry.path().join(SETUP_EXE);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Start VB-Audio's setup program with a Windows administrator prompt.
#[cfg(windows)]
pub fn launch_setup(exe: &Path) -> Result<(), String> {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let dir = exe.parent().ok_or("setup program has no parent folder")?;
    let file = HSTRING::from(exe.as_os_str());
    let cwd = HSTRING::from(dir.as_os_str());
    // "runas" makes Windows show its own elevation prompt for the setup program.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            &file,
            PCWSTR::null(),
            &cwd,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute reports success with a value greater than 32.
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(
            "The VB-CABLE setup did not start (the administrator prompt may have been declined)."
                .into(),
        )
    }
}

#[cfg(not(windows))]
pub fn launch_setup(_exe: &Path) -> Result<(), String> {
    Err("The bundled VB-CABLE setup is for Windows only.".into())
}
