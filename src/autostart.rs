//! Start the daemon at logon.
//!
//! From a normal terminal this uses the per-user `Run` key (no admin rights).
//! From an administrator terminal it registers a scheduled task that runs the
//! daemon elevated at logon instead, because keeping full-drive indexes current
//! means reading each volume's change journal, which needs administrator rights.
//!
//! Uses `bettergrepw.exe` (the windowless build of the CLI) when it sits next to
//! the running executable, so no console window appears at logon.

use std::process::Command;

use anyhow::{Result, anyhow};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};

use crate::config::Config;
use crate::win::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "bettergrep";
const TASK_NAME: &str = "bettergrep";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How autostart was set up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Per-user `Run` key: starts with normal rights.
    RunKey,
    /// Scheduled task: starts with administrator rights.
    ElevatedTask,
}

/// The program and arguments that autostart launches.
fn launch(cfg: &Config) -> Result<(String, String)> {
    let exe = std::env::current_exe()?;
    let windowless = exe.with_file_name("bettergrepw.exe");
    let program = if windowless.exists() { windowless } else { exe };
    Ok((program.display().to_string(), format!("--home \"{}\" daemon --quiet", cfg.home)))
}

pub fn command_line(cfg: &Config) -> Result<String> {
    let (program, args) = launch(cfg)?;
    Ok(format!("\"{program}\" {args}"))
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn open(access: u32) -> Option<Key> {
    let mut h: HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, access, &mut h) };
    (rc == ERROR_SUCCESS).then_some(Key(h))
}

/// Set up autostart: an elevated scheduled task when running as
/// administrator, the `Run` key otherwise.  Returns the command line and mode.
pub fn enable(cfg: &Config) -> Result<(String, Mode)> {
    let cmd = command_line(cfg)?;
    if crate::drive::is_elevated() {
        create_task(cfg)?;
        // One autostart is enough; the task replaces the Run value.
        let _ = remove_run_value();
        return Ok((cmd, Mode::ElevatedTask));
    }
    let mut h: HKEY = std::ptr::null_mut();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut h,
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(anyhow!("cannot open the Run key (error {rc})"));
    }
    let key = Key(h);
    let data = wide(&cmd);
    let rc = unsafe {
        RegSetValueExW(key.0, wide(VALUE_NAME).as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
    };
    if rc != ERROR_SUCCESS {
        return Err(anyhow!("cannot write the Run value (error {rc})"));
    }
    Ok((cmd, Mode::RunKey))
}

/// Remove autostart in either form.  True if anything was removed.
pub fn disable() -> Result<bool> {
    let removed_run = remove_run_value();
    if !task_exists() {
        return Ok(removed_run);
    }
    let out = schtasks(&["/Delete", "/F", "/TN", TASK_NAME])?;
    if !out.status.success() {
        return Err(anyhow!(
            "the elevated autostart task can only be removed from an administrator terminal ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(true)
}

/// What starts at logon, if anything.
pub fn status() -> Option<String> {
    if task_exists() {
        return Some(format!("scheduled task \"{TASK_NAME}\" (starts with administrator rights)"));
    }
    run_value()
}

fn remove_run_value() -> bool {
    let Some(key) = open(KEY_SET_VALUE) else { return false };
    unsafe { RegDeleteValueW(key.0, wide(VALUE_NAME).as_ptr()) == ERROR_SUCCESS }
}

fn run_value() -> Option<String> {
    let key = open(KEY_QUERY_VALUE)?;
    let name = wide(VALUE_NAME);
    let mut len = 0u32;
    let mut t = 0u32;
    unsafe { RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut t, std::ptr::null_mut(), &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize / 2 + 1];
    let rc =
        unsafe { RegQueryValueExW(key.0, name.as_ptr(), std::ptr::null(), &mut t, buf.as_mut_ptr().cast(), &mut len) };
    (rc == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..len as usize / 2]).trim_end_matches('\0').to_string())
}

fn schtasks(args: &[&str]) -> Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    Command::new("schtasks.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| anyhow!("cannot run schtasks.exe: {e}"))
}

fn task_exists() -> bool {
    schtasks(&["/Query", "/TN", TASK_NAME]).is_ok_and(|o| o.status.success())
}

/// Register the logon task from an XML definition: `schtasks /Create /SC
/// ONLOGON` alone would stop the daemon after the default 72-hour limit and on
/// battery power.
fn create_task(cfg: &Config) -> Result<()> {
    let (program, args) = launch(cfg)?;
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(name)) => format!("{domain}\\{name}"),
        (_, Ok(name)) => name,
        _ => return Err(anyhow!("cannot tell which user to start the task for")),
    };
    let xml = task_xml(&user, &program, &args);
    let path = std::env::temp_dir().join(format!("bettergrep-task-{}.xml", std::process::id()));
    // schtasks expects UTF-16 with a byte order mark for an XML that says so.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&path, bytes)?;
    let out = schtasks(&["/Create", "/F", "/TN", TASK_NAME, "/XML", &path.to_string_lossy()]);
    let _ = std::fs::remove_file(&path);
    let out = out?;
    if !out.status.success() {
        return Err(anyhow!("cannot create the scheduled task: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

fn task_xml(user: &str, program: &str, args: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let (user, program, args) = (esc(user), esc(program), esc(args));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>bettergrep: tracks file history and keeps the full-drive search indexes current.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{program}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::task_xml;

    #[test]
    fn task_runs_elevated_without_time_limit_and_escapes_paths() {
        let xml = task_xml(r"PC\me", r"C:\Tools & Co\bettergrepw.exe", r#"--home "C:\data" daemon --quiet"#);
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains(r"<Command>C:\Tools &amp; Co\bettergrepw.exe</Command>"));
        assert!(xml.contains(r"<Arguments>--home &quot;C:\data&quot; daemon --quiet</Arguments>"));
        assert!(xml.contains(r"<UserId>PC\me</UserId>"));
    }
}
