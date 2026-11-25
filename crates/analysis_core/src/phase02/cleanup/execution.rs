use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::super::ConfigError;

const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CHECK_OUTPUT: u64 = 1024 * 1024;
// Match the pinned Skylos bound for pytest JUnit receipts.
#[cfg(unix)]
const MAX_JUNIT_BYTES: u64 = 64 * 1024 * 1024;
const OUTPUT_TAIL_BYTES: u64 = 4096;
const OUTPUT_LIMIT_MESSAGE: &str = "test command exceeded its 1 MiB output budget";

pub(super) fn read(root: &Path, relative: &Path) -> Result<Vec<u8>, ConfigError> {
    validate_path(root, relative)?;
    #[cfg(unix)]
    {
        unix::read(root, relative, crate::source::MAX_SOURCE_BYTES)
    }
    #[cfg(not(unix))]
    {
        let path = root.join(relative);
        crate::preview_source::reject_symlink_components(root, &path).map_err(ConfigError::new)?;
        crate::preview_source::read_snapshot(&path, &relative.to_string_lossy(), None)
            .map(|(bytes, _)| bytes)
            .map_err(|issue| ConfigError::new(issue.message))
    }
}

fn read_junit(root: &Path) -> Result<Vec<u8>, ConfigError> {
    #[cfg(unix)]
    {
        let relative = Path::new("pytest.xml");
        validate_path(root, relative)?;
        unix::read(root, relative, MAX_JUNIT_BYTES)
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Err(ConfigError::new("guarded JUnit receipt reads require Unix"))
    }
}

pub(super) fn replace(
    root: &Path,
    relative: &Path,
    expected: &[u8],
    replacement: &[u8],
) -> Result<(), ConfigError> {
    validate_path(root, relative)?;
    #[cfg(unix)]
    {
        unix::replace(root, relative, expected, replacement)
    }
    #[cfg(not(unix))]
    {
        let _ = (expected, replacement);
        Err(ConfigError::new(
            "cleanup application and rollback require a platform with anchored file operations",
        ))
    }
}

fn validate_path(root: &Path, relative: &Path) -> Result<(), ConfigError> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ConfigError::new(
            "cleanup source must be a project-relative file",
        ));
    }
    let root_metadata = fs::symlink_metadata(root).map_err(ConfigError::new)?;
    if !root_metadata.is_dir() {
        return Err(ConfigError::new("cleanup root must be a regular directory"));
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsStr;
    use std::fs::Metadata;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;

    use rustix::fd::OwnedFd;
    use rustix::fs::{self as fd_fs, AtFlags, Mode, OFlags};

    use super::{ConfigError, File, Path, Read};
    use crate::preview_source::Stamp;
    use crate::source::MAX_SOURCE_BYTES;

    const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);

    fn directory(root: &Path, relative: &Path) -> Result<OwnedFd, ConfigError> {
        let mut handle =
            fd_fs::open(root, DIRECTORY_FLAGS, Mode::empty()).map_err(ConfigError::new)?;
        for part in relative
            .parent()
            .expect("validated relative path")
            .components()
        {
            handle = fd_fs::openat(&handle, part.as_os_str(), DIRECTORY_FLAGS, Mode::empty())
                .map_err(ConfigError::new)?;
        }
        Ok(handle)
    }

    fn open_source(dir: &OwnedFd, name: &OsStr) -> Result<(File, Metadata), ConfigError> {
        let named =
            fd_fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(ConfigError::new)?;
        if fd_fs::FileType::from_raw_mode(named.st_mode) != fd_fs::FileType::RegularFile {
            return Err(ConfigError::new(
                "cleanup source must be a regular file without a symlink",
            ));
        }
        // Nonblocking open prevents a racing replacement with a FIFO from waiting.
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file =
            File::from(fd_fs::openat(dir, name, flags, Mode::empty()).map_err(ConfigError::new)?);
        let metadata = file.metadata().map_err(ConfigError::new)?;
        if !metadata.is_file()
            || named.st_dev as u64 != metadata.dev()
            || named.st_ino as u64 != metadata.ino()
        {
            return Err(changed());
        }
        if metadata.nlink() != 1 {
            return Err(ConfigError::new(
                "cleanup source must have exactly one link",
            ));
        }
        Ok((file, metadata))
    }

    fn changed() -> ConfigError {
        ConfigError::new("cleanup source changed, preserving the current file")
    }

    fn snapshot(
        dir: &OwnedFd,
        name: &OsStr,
        max_bytes: u64,
    ) -> Result<(Vec<u8>, Metadata), ConfigError> {
        let (mut file, before) = open_source(dir, name)?;
        let stamp = Stamp::new(&before);
        let mut bytes = Vec::new();
        (&mut file)
            .take(max_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(ConfigError::new)?;
        if bytes.len() as u64 > max_bytes {
            return Err(ConfigError::new(format!(
                "cleanup file exceeds {} MiB",
                max_bytes / (1024 * 1024)
            )));
        }
        let after = file.metadata().map_err(ConfigError::new)?;
        let (_, live) = open_source(dir, name)?;
        if after.len() != bytes.len() as u64
            || Stamp::new(&after) != stamp
            || Stamp::new(&live) != stamp
        {
            return Err(changed());
        }
        Ok((bytes, before))
    }

    pub(super) fn read(
        root: &Path,
        relative: &Path,
        max_bytes: u64,
    ) -> Result<Vec<u8>, ConfigError> {
        let dir = directory(root, relative)?;
        let name = relative.file_name().expect("validated relative path");
        let (bytes, _) = snapshot(&dir, name, max_bytes)?;
        Ok(bytes)
    }

    struct Staged<'a> {
        dir: &'a OwnedFd,
        name: &'a OsStr,
        published: bool,
    }

    impl Drop for Staged<'_> {
        fn drop(&mut self) {
            if !self.published {
                let _ = fd_fs::unlinkat(self.dir, self.name, AtFlags::empty());
            }
        }
    }

    pub(super) fn replace(
        root: &Path,
        relative: &Path,
        expected: &[u8],
        replacement: &[u8],
    ) -> Result<(), ConfigError> {
        if replacement.len() as u64 > MAX_SOURCE_BYTES {
            return Err(ConfigError::new("cleanup replacement exceeds 1 MiB"));
        }
        let dir = directory(root, relative)?;
        let name = relative.file_name().expect("validated relative path");
        let (original, metadata) = snapshot(&dir, name, MAX_SOURCE_BYTES)?;
        if original != expected {
            return Err(changed());
        }
        // Use tempfile for a collision-resistant name, keeping creation, cleanup,
        // and publish relative to the proven directory handle.
        let mut staged = tempfile::Builder::new()
            .prefix(".skarmory-cleanup-")
            .rand_bytes(16)
            .disable_cleanup(true)
            .make_in(root, |path| {
                let name = path.file_name().expect("generated tempfile name");
                let flags = OFlags::WRONLY
                    | OFlags::CREATE
                    | OFlags::EXCL
                    | OFlags::NOFOLLOW
                    | OFlags::CLOEXEC;
                Ok(File::from(fd_fs::openat(
                    &dir,
                    name,
                    flags,
                    Mode::RUSR | Mode::WUSR,
                )?))
            })
            .map_err(ConfigError::new)?;
        let staging_name = staged
            .path()
            .file_name()
            .expect("generated tempfile name")
            .to_owned();
        let mut guard = Staged {
            dir: &dir,
            name: &staging_name,
            published: false,
        };
        let file = staged.as_file_mut();
        file.write_all(replacement).map_err(ConfigError::new)?;
        file.set_permissions(metadata.permissions())
            .map_err(ConfigError::new)?;
        file.sync_all().map_err(ConfigError::new)?;
        let staged_stamp = Stamp::new(&file.metadata().map_err(ConfigError::new)?);

        let live_dir = directory(root, relative)?;
        let held = fd_fs::fstat(&dir).map_err(ConfigError::new)?;
        let live = fd_fs::fstat(&live_dir).map_err(ConfigError::new)?;
        if (held.st_dev, held.st_ino) != (live.st_dev, live.st_ino) {
            return Err(ConfigError::new(
                "cleanup source directory changed, preserving the current file",
            ));
        }
        let (_, staged_metadata) = open_source(&dir, &staging_name)?;
        if Stamp::new(&staged_metadata) != staged_stamp {
            return Err(ConfigError::new("staged cleanup replacement changed"));
        }
        let (current, current_metadata) = snapshot(&dir, name, MAX_SOURCE_BYTES)?;
        if current != expected || Stamp::new(&current_metadata) != Stamp::new(&metadata) {
            return Err(changed());
        }
        fd_fs::renameat(&dir, &staging_name, &dir, name).map_err(ConfigError::new)?;
        guard.published = true;
        Ok(())
    }
}

pub(super) struct Check {
    argv: Vec<String>,
    pytest: bool,
}

#[derive(Serialize)]
pub(super) struct CheckResult {
    pub(super) status: &'static str,
    pub(super) observed: bool,
    exercised_change: Option<bool>,
    pub(super) evidence: &'static str,
    pub(super) returncode: Option<i32>,
    pub(super) error: Option<String>,
    count: Option<usize>,
    output: String,
}

impl CheckResult {
    pub(super) fn skipped() -> Self {
        Self {
            status: "skipped",
            observed: false,
            exercised_change: None,
            evidence: "none",
            returncode: None,
            error: None,
            count: None,
            output: String::new(),
        }
    }

    fn incomplete(error: impl ToString) -> Self {
        Self {
            status: "incomplete",
            error: Some(error.to_string()),
            ..Self::skipped()
        }
    }
}

impl Check {
    pub(super) fn parse(command: &str) -> Result<Self, ConfigError> {
        let invalid =
            || ConfigError::new("test command is empty or contains unsupported shell syntax");
        if command.contains([';', '&', '|', '<', '>', '`', '$', '\n', '\r', '\0']) {
            return Err(invalid());
        }
        let argv = shlex::split(command).ok_or_else(invalid)?;
        let program = argv
            .first()
            .filter(|arg| !arg.is_empty())
            .ok_or_else(invalid)?;
        let program = program
            .rsplit(['/', '\\'])
            .next()
            .expect("nonempty program");
        let program = program.strip_suffix(".exe").unwrap_or(program);
        let pytest = matches!(program, "pytest" | "py.test")
            || (program.starts_with("python")
                && argv.get(1).is_some_and(|arg| arg == "-m")
                && argv.get(2).is_some_and(|arg| arg == "pytest"));
        Ok(Self { argv, pytest })
    }

    pub(super) fn run(&self, root: &Path) -> CheckResult {
        match self.execute(root) {
            Ok(result) => result,
            Err(error) => CheckResult::incomplete(error),
        }
    }

    fn execute(&self, root: &Path) -> Result<CheckResult, ConfigError> {
        let mut log = tempfile::tempfile().map_err(ConfigError::new)?;
        let program = Path::new(&self.argv[0]);
        let mut command = if program.is_relative() && program.components().count() > 1 {
            Command::new(root.join(program))
        } else {
            Command::new(program)
        };
        command
            .args(&self.argv[1..])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(ConfigError::new)?)
            .stderr(log.try_clone().map_err(ConfigError::new)?)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let junit = if self.pytest {
            let directory = tempfile::tempdir().map_err(ConfigError::new)?;
            let path = directory.path().join("pytest.xml");
            command.arg("--junitxml").arg(path).args([
                "-o",
                "junit_family=xunit1",
                "-o",
                "junit_logging=no",
            ]);
            Some(directory)
        } else {
            None
        };

        let mut child = command
            .spawn()
            .map_err(|error| ConfigError::new(format!("test command could not start: {error}")))?;
        let status = checked_wait(&mut child, &log);
        let size = log.metadata().map_err(ConfigError::new)?.len();
        let status = status.and_then(|status| {
            if size > MAX_CHECK_OUTPUT {
                Err(ConfigError::new(OUTPUT_LIMIT_MESSAGE))
            } else {
                Ok(status)
            }
        });
        let mut tail = Vec::new();
        log.seek(SeekFrom::Start(size.saturating_sub(OUTPUT_TAIL_BYTES)))
            .and_then(|_| (&mut log).take(OUTPUT_TAIL_BYTES).read_to_end(&mut tail))
            .map_err(ConfigError::new)?;
        let output = String::from_utf8_lossy(&tail).into_owned();
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                return Ok(CheckResult {
                    output,
                    ..CheckResult::incomplete(error)
                });
            }
        };
        let mut result = CheckResult {
            returncode: status.code(),
            evidence: "exit_code",
            output,
            ..CheckResult::skipped()
        };
        if let Some(junit) = junit {
            let receipt = read_junit(junit.path())
                .ok()
                .and_then(|bytes| junit_counts(&bytes));
            match receipt {
                Some((count, skipped, failures)) => {
                    result.evidence = "pytest_junit";
                    result.count = Some(count);
                    result.observed = count > skipped;
                    result.status = if failures > 0 {
                        "failed"
                    } else if result.observed && status.success() {
                        "passed"
                    } else {
                        result.error =
                            Some("test command did not report successful executed tests".into());
                        "incomplete"
                    };
                }
                None => {
                    result.error =
                        Some("pytest did not produce a readable fresh JUnit receipt".into());
                    result.status = "incomplete";
                }
            }
        } else if status.code().is_none() {
            result.status = "incomplete";
            result.error = Some("test command stopped without an exit code".into());
        } else {
            result.status = if status.success() { "passed" } else { "failed" };
        }
        Ok(result)
    }
}

fn checked_wait(child: &mut Child, log: &File) -> Result<ExitStatus, ConfigError> {
    let start = Instant::now();
    loop {
        let state = (|| {
            if log.metadata()?.len() > MAX_CHECK_OUTPUT {
                return Err(std::io::Error::other(OUTPUT_LIMIT_MESSAGE));
            }
            if start.elapsed() >= CHECK_TIMEOUT {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "test command timed out",
                ));
            }
            child.try_wait()
        })();
        match state {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                stop(child);
                return Err(ConfigError::new(error));
            }
        }
    }
}

fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        if let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn junit_counts(bytes: &[u8]) -> Option<(usize, usize, usize)> {
    // Read the single suite emitted by the directly invoked pytest command.
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let text = if text.starts_with("<?xml ") {
        text.split_once("?>")?.1.trim_start()
    } else {
        text
    };
    let package = text
        .strip_prefix("<testsuites ")?
        .strip_suffix("</testsuites>")?;
    let (_, suite) = package.split_once('>')?;
    let suite = suite.trim().strip_prefix("<testsuite ")?;
    let (header, cases) = if let Some(header) = suite.strip_suffix("/>") {
        (header, "")
    } else {
        suite.strip_suffix("</testsuite>")?.split_once('>')?
    };
    let number = |name| -> Option<usize> {
        header
            .split_once(&format!(" {name}=\""))?
            .1
            .split_once('"')?
            .0
            .parse()
            .ok()
    };
    let declared = number("tests")?;
    let count = cases.matches("<testcase ").count();
    let skipped = number("skipped")?;
    let failed = number("failures")?;
    let errors = number("errors")?;
    let failures = failed.checked_add(errors)?;
    // Pytest writes a second testcase for a teardown error after a call failure.
    (count.checked_sub(declared)? <= errors
        && skipped.checked_add(failures)? <= count
        && cases.matches("<skipped ").count() == skipped
        && cases.matches("<failure ").count() == failed
        && cases.matches("<error ").count() == errors)
        .then_some((count, skipped, failures))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pytest_receipt_requires_cases_matching_the_suite_counts() {
        let cases = r#"<testcase name="skip"><skipped /></testcase><testcase name="pass" />"#;
        let receipt = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><testsuites name="pytest tests"><testsuite name="pytest" errors="0" failures="0" skipped="1" tests="2">{cases}</testsuite></testsuites>"#
        );
        assert_eq!(junit_counts(receipt.as_bytes()), Some((2, 1, 0)));
        assert_eq!(junit_counts(receipt.replace(cases, "").as_bytes()), None);
        assert_eq!(
            junit_counts(br#"<testsuites name="pytest tests"><testsuite name="pytest" errors="0" failures="0" skipped="0" tests="0" /></testsuites>"#),
            Some((0, 0, 0)),
        );
    }

    #[test]
    fn pytest_reports_test_and_teardown_failures_as_separate_cases() {
        let receipt = br#"<testsuites name="pytest tests"><testsuite name="pytest" errors="1" failures="1" skipped="0" tests="1"><testcase name="test_example"><failure message="assert False" /></testcase><testcase name="test_example"><error message="failed on teardown" /></testcase></testsuite></testsuites>"#;
        assert_eq!(junit_counts(receipt), Some((2, 0, 2)));
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_writes_refuse_linked_or_changed_sources() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let source = root.join("source.py");
        fs::write(&source, b"original").unwrap();
        let linked = Path::new("linked.py");
        fs::hard_link(&source, root.join(linked)).unwrap();
        assert!(read(root, linked).is_err());
        assert!(replace(root, linked, b"original", b"replacement").is_err());
        fs::remove_file(root.join(linked)).unwrap();
        symlink(&source, root.join(linked)).unwrap();
        assert!(read(root, linked).is_err());
        assert!(replace(root, linked, b"original", b"replacement").is_err());
        assert!(replace(root, Path::new("source.py"), b"stale", b"replacement").is_err());
        assert_eq!(fs::read(source).unwrap(), b"original");
    }
}
