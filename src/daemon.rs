use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Read PID from file
///
/// # Arguments
/// * `pid_file` - Path to the PID file containing the daemon's process ID
///
/// # Returns
/// Returns the parsed PID as i32, or an error if the file can't be read or parsed
pub fn read_pid(pid_file: &Path) -> Result<i32> {
    let pid_str = fs::read_to_string(pid_file)
        .with_context(|| format!("Failed to read PID file: {}", pid_file.display()))?;

    pid_str.trim().parse().context("Invalid PID in file")
}

/// Stop daemon by PID from file
///
/// # Arguments
/// * `pid_file` - Path to the PID file containing the daemon's process ID
///
/// # Returns
/// Returns Ok(()) if daemon was successfully stopped
/// Returns Err if PID file doesn't exist, invalid PID, or process couldn't be killed
///
/// # Examples
///
/// ```no_run
/// # use klava::daemon::stop_daemon;
/// # use std::path::Path;
/// let pid_file = Path::new("/tmp/klava.pid");
/// if let Err(e) = stop_daemon(pid_file) {
///     eprintln!("Failed to stop daemon: {}", e);
///     std::process::exit(1);
/// }
/// ```
pub fn stop_daemon(pid_file: &Path) -> Result<()> {
    let pid = read_pid(pid_file)?;

    #[cfg(unix)]
    {
        // Try to kill gracefully first
        let output = Command::new("kill").arg(pid.to_string()).output();

        if output.is_ok() && output.unwrap().status.success() {
            fs::remove_file(pid_file)?;
            return Ok(());
        }

        // Force kill if graceful kill didn't work
        let output = Command::new("kill").arg("-9").arg(pid.to_string()).output();

        if output.is_ok() && output.unwrap().status.success() {
            fs::remove_file(pid_file)?;
            return Ok(());
        }

        Err(anyhow::anyhow!("Failed to stop daemon (PID: {})", pid))
    }

    #[cfg(not(unix))]
    {
        return Err(anyhow::anyhow!(
            "Daemon stop is only supported on Unix systems"
        ));
    }
}

/// Check if daemon is running
///
/// # Arguments
/// * `pid_file` - Path to the PID file to check
///
/// # Returns
/// Returns `Ok(true)` if daemon is running
/// Returns `Ok(false)` if daemon is not running (PID file doesn't exist or process doesn't exist)
/// Returns `Err` if there's an error reading the PID file
///
/// # Examples
///
/// ```no_run
/// # use klava::daemon::check_status;
/// # use std::path::Path;
/// let pid_file = Path::new("/tmp/klava.pid");
/// if check_status(pid_file).is_ok() {
///     println!("Daemon is running");
/// } else {
///     println!("Daemon is not running");
/// }
/// ```
pub fn check_status(pid_file: &Path) -> Result<bool> {
    // Check if PID file exists
    if !pid_file.exists() {
        return Ok(false);
    }

    let pid = read_pid(pid_file)?;

    #[cfg(unix)]
    {
        // Check if process exists by checking if it can be found with pgrep
        let output = Command::new("pgrep")
            .arg("-P")
            .arg(pid.to_string())
            .output();

        let exists = output.is_ok() && output.unwrap().status.success();

        if exists {
            println!("✓ Daemon is running (PID: {})", pid);
            println!("  PID file: {}", pid_file.display());
        } else {
            println!("✗ Daemon is not running");
            println!(
                "  Stale PID file found: {} (PID: {})",
                pid_file.display(),
                pid
            );
        }

        Ok(exists)
    }

    #[cfg(not(unix))]
    {
        Ok(false)
    }
}

/// Setup PID directory
///
/// Creates parent directory for PID file if it doesn't exist
fn setup_pid_directory(pid_file: &Path) -> Result<()> {
    let pid_parent = pid_file
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    fs::create_dir_all(pid_parent)?;
    Ok(())
}

/// Start the daemon process
///
/// This function daemonizes the current process and setup standard file descriptors
/// to log output to a file. It does NOT fork again - the caller should continue
/// with daemon operations after this returns.
///
/// # Arguments
/// * `log_path` - Path where logs should be written to
/// * `pid_file` - Path to the PID file
///
/// # Returns
/// Returns `Ok(())` if daemonization setup succeeds
///
/// # Note
/// The calling process should remain running (do not change code after this call)
/// - daemonization is applied by the daemonize crate
#[cfg(unix)]
pub fn prepare_daemon(log_path: &Path, pid_file: &Path) -> Result<()> {
    setup_pid_directory(pid_file)?;

    let daemonize = daemonize::Daemonize::new()
        .pid_file(pid_file)
        .working_directory(std::env::current_dir()?)
        .stdout(std::fs::File::create(log_path)?)
        .stderr(std::fs::File::create(log_path)?)
        .umask(0o027);

    match daemonize.start() {
        Ok(_) => {
            // Successfully forked - this is now running in background
            let pid = std::process::id();
            println!("✓ Daemon started (PID: {})", pid);
        }
        Err(e) => {
            return Err(anyhow::anyhow!("Failed to daemonize: {}", e));
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn prepare_daemon(log_path: &Path, pid_file: &Path) -> Result<()> {
    Err(anyhow::anyhow!(
        "Daemon mode is only supported on Unix systems. Use 'klava up' to run in foreground mode."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn test_mock_pid_file_logic() {
        let temp_dir = std::env::temp_dir();
        let pid_file = temp_dir.join("test_klava.pid");

        fs::write(&pid_file, "12345").unwrap();
        assert!(pid_file.exists());

        // Would call stop_daemon or check_status in unittests
        // For now, just verify file was created
        assert_eq!(fs::read_to_string(&pid_file).unwrap(), "12345");
        fs::remove_file(&pid_file).unwrap();
    }
}
