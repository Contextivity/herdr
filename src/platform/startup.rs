//! Private, single-use launch files. Bulk shell input never enters a pre-ready PTY.
use std::io;
#[cfg(unix)]
use std::io::Write;
use std::path::PathBuf;

pub(crate) struct StartupScript {
    directory: PathBuf,
    pub(crate) source_command: String,
    owns_files: bool,
}

impl StartupScript {
    pub(crate) fn for_interactive(shell: &str, command: &str) -> io::Result<Option<Self>> {
        #[cfg(unix)]
        {
            if matches!(
                shell.trim_start_matches('-'),
                "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh"
            ) {
                Self::create(shell, command, &[]).map(Some)
            } else {
                Ok(None)
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (shell, command);
            Ok(None)
        }
    }

    #[cfg(unix)]
    pub(crate) fn create(shell: &str, command: &str, preparation: &[String]) -> io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let shell = shell.trim_start_matches('-');
        if !matches!(shell, "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh") {
            return Err(io::Error::other(
                "script-backed startup requires a POSIX-compatible shell",
            ));
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "herdr-start-{}-{stamp:x}-{sequence:x}",
            std::process::id()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let script_path = directory.join("launch");
        let quoted = quote(&script_path.to_string_lossy());
        let script = Self {
            directory,
            source_command: format!(". {quoted}"),
            owns_files: true,
        };
        // Darwin's canonical queue is small. Leave ample room for newline and
        // optional terminal framing; reject an excessively long TMPDIR.
        if script.source_command.len() > 256 {
            return Err(io::Error::other(
                "startup source path exceeds safe PTY input size",
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(script_path)?;
        // Unlink before executing anything: even a separately repeated source
        // cannot replay a completed task. The daemon independently forbids resend.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(script.directory.join("finished"))?;
        writeln!(file, "command /bin/rm -- {quoted} || return")?;
        writeln!(file, "{{")?;
        for item in preparation {
            writeln!(file, "{{ {item}\n}} &&")?;
        }
        writeln!(file, "{command}\n}}")?;
        writeln!(
            file,
            "printf '%s\\n' \"$?\" >| {}",
            quote(&script.directory.join("finished").to_string_lossy())
        )?;
        file.sync_all()?;
        Ok(script)
    }

    #[cfg(not(unix))]
    pub(crate) fn create(
        _shell: &str,
        _command: &str,
        _preparation: &[String],
    ) -> io::Result<Self> {
        Err(io::Error::other(
            "script-backed startup is unsupported on this platform",
        ))
    }

    pub(crate) fn ticket(&self) -> String {
        self.directory.to_string_lossy().into_owned()
    }

    pub(crate) fn finished(&self) -> bool {
        std::fs::read_to_string(self.directory.join("finished"))
            .ok()
            .is_some_and(|value| value.trim().parse::<u8>().is_ok())
    }

    pub(crate) fn from_handoff(directory: PathBuf, source_command: String) -> Self {
        Self {
            directory,
            source_command,
            owns_files: false,
        }
    }

    pub(crate) fn assume_handoff_ownership(&mut self) {
        self.owns_files = true;
    }

    pub(crate) fn relinquish_handoff_ownership(&mut self) {
        self.owns_files = false;
    }
}

#[cfg(unix)]
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl Drop for StartupScript {
    fn drop(&mut self) {
        if !self.owns_files {
            return;
        }
        // Never recursively sweep TMPDIR or other tickets.
        for name in ["launch", "finished"] {
            let _ = std::fs::remove_file(self.directory.join(name));
        }
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn startup_completion_survives_noclobber() {
        for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
            let script = StartupScript::create("sh", "false", &[]).unwrap();
            let output = std::process::Command::new(shell)
                .args(["-c", &format!("set -C; {}", script.source_command)])
                .output()
                .unwrap();
            assert!(
                script.finished(),
                "{shell}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(script.directory.join("finished"))
                    .unwrap()
                    .trim(),
                "1"
            );
        }
    }

    #[test]
    fn startup_script_is_private_single_use_and_preserves_quoted_arguments() {
        for size in [1000, 8000] {
            let payload = format!("{} 'quoted' $literal ; &", "x".repeat(size));
            let output = std::env::temp_dir()
                .join(format!("herdr-startup-test-{}-{size}", std::process::id()));
            let command = format!(
                "printf '%s\\n' {} >> {}",
                quote(&payload),
                quote(&output.to_string_lossy())
            );
            let script = StartupScript::create("zsh", &command, &[]).unwrap();
            assert_eq!(
                std::fs::metadata(&script.directory)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(script.directory.join("launch"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            let first = std::process::Command::new("/bin/zsh")
                .args(["-fc", &script.source_command])
                .env("PATH", "/nonexistent")
                .output()
                .unwrap();
            assert!(first.status.success());
            assert!(script.finished());
            let second = std::process::Command::new("/bin/zsh")
                .args(["-fc", &script.source_command])
                .env("PATH", "/nonexistent")
                .output()
                .unwrap();
            assert!(!second.status.success());
            assert_eq!(
                std::fs::read_to_string(&output).unwrap(),
                format!("{payload}\n")
            );
            std::fs::remove_file(output).unwrap();
            let directory = script.directory.clone();
            drop(script);
            assert!(!directory.exists());
        }
    }

    #[test]
    fn imported_startup_script_does_not_clean_up_before_handoff_commit() {
        let mut original = StartupScript::create("sh", "true", &[]).unwrap();
        let directory = original.directory.clone();
        let mut imported =
            StartupScript::from_handoff(directory.clone(), original.source_command.clone());
        drop(imported);
        assert!(directory.join("launch").exists());
        imported = StartupScript::from_handoff(directory.clone(), original.source_command.clone());
        imported.assume_handoff_ownership();
        drop(imported);
        assert!(!directory.exists());
        original.relinquish_handoff_ownership();
    }
}
