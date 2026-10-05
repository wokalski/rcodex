use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn inspect(path: &Path, fake_bin: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rcodex"));
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("__git")
        .arg(path);
    if let Some(bin) = fake_bin {
        command.env("PATH", bin);
    }
    let output = command.output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn git_inspection_reports_changes_without_touching_index() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project ' 日本語");
    fs::create_dir(&project).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&project)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "inspector"]);
    fs::write(project.join("tracked.txt"), "staged\n").unwrap();
    git(&["add", "tracked.txt"]);
    fs::write(project.join("tracked.txt"), "unstaged edit\n").unwrap();
    fs::write(project.join("scratch.txt"), "new file\n").unwrap();
    let index = fs::read(project.join(".git/index")).unwrap();
    let status = inspect(&project, None).unwrap();
    assert!(status.contains("inspector"), "{status}");
    assert!(status.contains("AM tracked.txt"), "{status}");
    assert!(status.contains("?? scratch.txt"), "{status}");
    assert_eq!(fs::read(project.join(".git/index")).unwrap(), index);
    assert!(!project.join(".git/index.lock").exists());
    assert!(inspect(root.path(), None).is_err());
}

#[test]
fn git_inspection_bounds_output_and_runtime() {
    let root = tempfile::tempdir().unwrap();
    let git = root.path().join("git");
    fs::write(&git, "#!/bin/sh\nwhile :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n'; done\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
    let status = inspect(root.path(), Some(root.path())).unwrap();
    assert!(status.ends_with("[Git output truncated at 64 KiB]"));
    assert!(status.len() < 66_000);
    fs::write(&git, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
    let started = Instant::now();
    assert!(
        inspect(root.path(), Some(root.path()))
            .unwrap_err()
            .contains("timed out")
    );
    assert!(started.elapsed() < Duration::from_secs(15));
}
