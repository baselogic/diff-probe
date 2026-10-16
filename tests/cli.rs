use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn cli_reports_colored_context_and_exit_codes() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("diff-probe-{stamp}-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    let _cleanup = Cleanup(root.clone());
    fs::write(root.join("source.txt"), b"before\nold\nafter\n").unwrap();
    fs::write(root.join("change.patch"), b"--- a/source.txt\n+++ b/source.txt\n@@ -2 +2 @@\n-old\n+new\n").unwrap();
    let run = |color: &str| Command::new(env!("CARGO_BIN_EXE_diff-probe"))
        .arg(root.join("change.patch")).arg("--root").arg(&root)
        .args(["--context", "1", "--color", color])
        .output().unwrap();
    let plain = run("never");
    assert_eq!(plain.status.code(), Some(0));
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(text.contains("MATCH"));
    assert!(text.contains("before"));
    assert!(text.contains("after"));
    assert!(!text.contains("\x1b["));
    let colored = run("always");
    assert_eq!(colored.status.code(), Some(0));
    assert!(colored.stdout.windows(5).any(|w| w == b"\x1b[31m"));
    // Validate creation targets and modified-file destination paths as well.
    for malicious in [
        b"--- /dev/null\n+++ b/../outside.txt\n@@ -0,0 +1 @@\n+new\n".as_slice(),
        b"--- a/source.txt\n+++ b/../outside.txt\n@@ -2 +2 @@\n-old\n+new\n",
    ] {
        fs::write(root.join("change.patch"), malicious).unwrap();
        assert_eq!(run("never").status.code(), Some(2));
    }
    // Reject excessive output before writing even the report header.
    let mut oversized = Vec::new();
    for _ in 0..60 {
        oversized.extend_from_slice(b"--- a/source.txt\n+++ b/source.txt\n@@ -2 +2 @@\n-old\n+new\n");
    }
    fs::write(root.join("change.patch"), oversized).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_diff-probe"))
        .arg(root.join("change.patch")).arg("--root").arg(&root)
        .args(["--context", "1000", "--color", "never"])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let help = Command::new(env!("CARGO_BIN_EXE_diff-probe"))
        .arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8(help.stdout).unwrap().starts_with("Usage: diff-probe PATCH"));

    let invalid = Command::new(env!("CARGO_BIN_EXE_diff-probe"))
        .arg("--not-an-option").output().unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8(invalid.stderr).unwrap().contains("diff-probe: unknown option:"));
}

#[cfg(unix)]
#[test]
fn unix_paths_symlinks_and_fifos() {
    use std::os::unix::fs::symlink;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let base = std::env::temp_dir().join(format!("diff-probe-unix-{stamp}-{}", std::process::id()));
    fs::create_dir(&base).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    let _cleanup = Cleanup(base.clone());
    let root = base.join("root");
    let outside = base.join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(root.join("report:2026"), b"old\n").unwrap();
    let patch = base.join("input.diff");
    let run = || Command::new(env!("CARGO_BIN_EXE_diff-probe"))
        .arg(&patch).arg("--root").arg(&root).arg("--color").arg("never")
        .output().unwrap();

    fs::write(&patch, b"--- a/report:2026\n+++ b/report:2026\n@@ -1 +1 @@\n-old\n+new\n").unwrap();
    assert_eq!(run().status.code(), Some(0));

    symlink(&outside, root.join("link")).unwrap();
    fs::write(&patch, b"--- /dev/null\n+++ b/link/new.txt\n@@ -0,0 +1 @@\n+new\n").unwrap();
    assert_eq!(run().status.code(), Some(2));

    // A FIFO supplied as PATCH must fail rather than wait for another process to open it.
    let fifo = base.join("input.fifo");
    assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    let mut child = Command::new(env!("CARGO_BIN_EXE_diff-probe"))
        .arg(&fifo).arg("--root").arg(&root)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(2));
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("opening a FIFO blocked instead of rejecting a non-regular file");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
