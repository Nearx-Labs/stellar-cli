/*
`doctor` reports on things it discovers outside the process: which Stellar CLI
executables are on `PATH`, and which CLI last checked for a new release and
wrote the shared version cache. Both inputs are injectable -- `PATH` like in
`plugin.rs`, and the cache via `STELLAR_DATA_HOME` -- so the reporting can be
exercised end to end.

Unix only: the fake CLIs are shell scripts that need an execute bit.
*/

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use soroban_cli::commands::version::pkg;
use soroban_test::TestEnv;

/// A stand-in CLI on `PATH` that answers version queries and nothing else.
///
/// `supports_only_version` distinguishes a current CLI from one old enough to
/// reject `version --only-version` -- the releases that cause the version
/// confusion these checks exist to surface only answer `--version`.
fn write_fake_cli(dir: &Path, name: &str, version: &str, supports_only_version: bool) {
    let only_version = if supports_only_version {
        format!("echo \"{version}\"")
    } else {
        "echo \"error: unexpected argument '--only-version' found\" >&2; exit 2".to_string()
    };

    let script = format!(
        "#!/bin/sh\n\
         case \"$*\" in\n\
           \"version --only-version\") {only_version} ;;\n\
           \"--version\") echo \"stellar {version} (0000000000000000000000000000000000000000)\" ;;\n\
           *) echo \"unexpected args: $*\" >&2; exit 1 ;;\n\
         esac\n"
    );

    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A CLI on `PATH` that answers version queries and reports, to `stdin_log`,
/// whether it was handed readable input or EOF.
///
/// Records `read:<line>` when a read succeeds and `eof` when it does not, so a
/// probe that inherited the caller's stdin is distinguishable from one given
/// `/dev/null` by what it wrote, not by how long it took.
fn write_stdin_reading_cli(dir: &Path, name: &str, version: &str, stdin_log: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         if IFS= read -r line; then\n\
           echo \"read:$line\" >> \"{log}\"\n\
         else\n\
           echo \"eof\" >> \"{log}\"\n\
         fi\n\
         case \"$*\" in\n\
           \"version --only-version\") echo \"{version}\" ;;\n\
           *) echo \"unexpected args: $*\" >&2; exit 1 ;;\n\
         esac\n",
        log = stdin_log.to_string_lossy()
    );

    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A CLI on `PATH` that cannot be asked for its version -- an install too
/// broken to answer either version query.
fn write_unrunnable_cli(dir: &Path, name: &str) {
    let path = dir.join(name);
    fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A CLI on `PATH` that never answers -- it outlives `doctor`'s probe timeout,
/// so the probe must time out rather than the process exiting on its own.
///
/// Appends its PID to `pid_file` with `>>` before looping, because
/// `installed_version` may spawn this script twice -- once for
/// `version --only-version`, once for `--version` -- and each spawn is a
/// fresh process whose PID the test needs to check afterward.
///
/// Loops on the shell's own `:` builtin rather than shelling out to `sleep`:
/// `doctor` sets `PATH` to exactly the directory under test (see `doctor`
/// below), so this script inherits that same narrowed `PATH` and could not
/// resolve an external `sleep` binary either -- it would exit almost
/// immediately instead of hanging.
fn write_hanging_cli(dir: &Path, name: &str, pid_file: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         echo \"$$\" >> \"{}\"\n\
         while :; do :; done\n",
        pid_file.to_string_lossy()
    );

    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A directory under the sandbox, canonicalized so that the `PATH` these tests
/// set and the paths they expect back are one spelling of it.
///
/// `doctor` echoes the `PATH` entry it found an executable under, so any
/// consistent spelling would do -- but only if it is consistent. On macOS the
/// temporary directory sits under `/var/folders`, a symlink to `/private/var`,
/// and mixing the two forms compares them unequal as strings.
fn empty_dir(sandbox: &TestEnv, name: &str) -> PathBuf {
    let dir = sandbox.dir().join(name);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap_or(dir)
}

/// The path `doctor` reports as its own executable, and compares cache entries
/// against.
///
/// `running_binary` leaves `current_exe` as the process was started from, and
/// the two platforms hand that over differently: Linux resolves it through
/// `/proc/self/exe` before the CLI sees it, macOS gives the invoked path.
/// Canonicalizing here matches Linux exactly, and matches macOS as long as no
/// symlink stands in the cargo target path -- which is what makes the two forms
/// the same string.
fn running_binary() -> String {
    let path = assert_cmd::cargo::cargo_bin("stellar");
    let path = path.canonicalize().unwrap_or(path);
    path.to_string_lossy().into_owned()
}

/// A path beside the running executable, where the crate's other binary name
/// lands when one install ships both.
fn sibling_binary(name: &str) -> String {
    PathBuf::from(running_binary())
        .with_file_name(name)
        .to_string_lossy()
        .into_owned()
}

/// Seed the shared version cache with a given writer, and return the data home
/// that holds it.
///
/// `latest_check_time` is irrelevant to what is asserted here: `doctor` calls
/// `has_available_upgrade` with caching off, so it always attempts a refresh.
/// The seeded writer is still what gets reported, because `doctor` reads it
/// before that refresh can overwrite it.
fn seed_cache_writer(sandbox: &TestEnv, writer: serde_json::Value) -> PathBuf {
    let data_home = empty_dir(sandbox, "cache-data-home");

    let cache = serde_json::json!({
        "latest_check_time": "2026-08-04T10:00:00Z",
        "max_stable_version": "27.1.0",
        "max_version": "27.1.0",
        "last_checked_by": writer,
    });

    fs::write(
        data_home.join("upgrade_check.json"),
        serde_json::to_string(&cache).unwrap(),
    )
    .unwrap();

    data_home
}

/// `doctor` with `PATH` and the version cache pointed at test-controlled state.
///
/// `path` is a whole `PATH` value, not a single directory: a multi-entry one is
/// several paths joined by `:` and is no longer a path itself.
fn doctor(sandbox: &TestEnv, path: impl AsRef<OsStr>, data_home: &Path) -> Command {
    let mut cmd = sandbox.new_assert_cmd("doctor");
    cmd.env("PATH", path).env("STELLAR_DATA_HOME", data_home);
    cmd
}

#[test]
fn reports_the_running_executable_and_a_lone_install() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "one-install");
    let data_home = empty_dir(&sandbox, "data-home");
    write_fake_cli(&bin_dir, "stellar", "27.1.0", true);

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(format!(
            "Running executable: {}",
            running_binary()
        )))
        .stderr(contains("Only one Stellar CLI found on PATH"))
        // The lone install is listed with its version too: "Running executable"
        // is not necessarily the entry `PATH` resolves by name, and carries no
        // version of its own.
        .stderr(contains(format!(
            "- {} (27.1.0)",
            bin_dir.join("stellar").to_string_lossy()
        )));
}

#[test]
fn warns_when_the_only_install_cannot_report_a_version() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "one-install-unreadable");
    let data_home = empty_dir(&sandbox, "data-home");
    write_unrunnable_cli(&bin_dir, "stellar");

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Found one Stellar CLI on PATH, but it did not report a version",
        ))
        // Being alone is not a clean bill of health. The line a healthy lone
        // install gets would put a success marker above one reading "unknown
        // version", and the same silent executable warns as soon as it has
        // company.
        .stderr(contains("Only one Stellar CLI found on PATH").not())
        .stderr(contains(format!(
            "- {} (unknown version)",
            bin_dir.join("stellar").to_string_lossy()
        )));
}

#[test]
fn reports_when_no_cli_is_on_path() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "no-installs");
    let data_home = empty_dir(&sandbox, "data-home");

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains("No Stellar CLI found on PATH"))
        // Nothing was found, so claiming a single install would be a lie.
        .stderr(contains("Only one Stellar CLI").not());
}

#[test]
fn does_not_warn_when_both_binary_names_report_the_same_version() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "two-installs-agreeing");
    let data_home = empty_dir(&sandbox, "data-home");
    // What an ordinary install looks like: one crate, two binary names.
    write_fake_cli(&bin_dir, "stellar", "27.1.0", true);
    write_fake_cli(&bin_dir, "soroban", "27.1.0", true);

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "2 Stellar CLI executables on PATH, all reporting 27.1.0",
        ))
        .stderr(contains("different versions").not());
}

#[test]
fn warns_when_installs_report_different_versions() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "two-installs-disagreeing");
    let data_home = empty_dir(&sandbox, "data-home");
    write_fake_cli(&bin_dir, "stellar", "27.1.0", true);
    // Old enough to only answer `--version`, which is the fallback path.
    write_fake_cli(&bin_dir, "soroban", "22.8.0", false);

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Found 2 Stellar CLI executables on PATH reporting different versions",
        ))
        .stderr(contains(format!(
            "- {} (27.1.0)",
            bin_dir.join("stellar").to_string_lossy()
        )))
        .stderr(contains(format!(
            "- {} (22.8.0)",
            bin_dir.join("soroban").to_string_lossy()
        )));
}

#[test]
fn does_not_blame_differing_versions_when_a_version_could_not_be_read() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "two-installs-unreadable");
    let data_home = empty_dir(&sandbox, "data-home");
    // Neither answers, so nothing was observed to disagree -- the report must
    // say the versions are unknown, not that they differ.
    write_unrunnable_cli(&bin_dir, "stellar");
    write_unrunnable_cli(&bin_dir, "soroban");

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Found 2 Stellar CLI executables on PATH; none of them reported a version",
        ))
        .stderr(contains("different versions").not())
        .stderr(contains(format!(
            "- {} (unknown version)",
            bin_dir.join("stellar").to_string_lossy()
        )));
}

#[test]
fn reports_a_disagreement_even_when_another_executable_is_unreadable() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "disagreeing-and-unreadable");
    let second_dir = empty_dir(&sandbox, "disagreeing-and-unreadable-2");
    let data_home = empty_dir(&sandbox, "data-home");
    write_fake_cli(&bin_dir, "stellar", "27.1.0", true);
    write_fake_cli(&bin_dir, "soroban", "22.8.0", false);
    write_unrunnable_cli(&second_dir, "stellar");

    let path = format!(
        "{}:{}",
        bin_dir.to_string_lossy(),
        second_dir.to_string_lossy()
    );

    doctor(&sandbox, &path, &data_home)
        .assert()
        .success()
        // An observed disagreement is a fact; a failed probe alongside it does
        // not soften it. It does not join it either: only two executables were
        // heard from, so only two can be said to disagree.
        .stderr(contains(
            "Found 3 Stellar CLI executables on PATH; the 2 that reported a version do not \
             agree (1 could not be asked)",
        ));
}

#[test]
fn reports_the_agreement_among_the_executables_that_answered() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "agreeing-and-unreadable");
    let second_dir = empty_dir(&sandbox, "agreeing-and-unreadable-2");
    let data_home = empty_dir(&sandbox, "data-home");
    write_fake_cli(&bin_dir, "stellar", "27.1.0", true);
    write_fake_cli(&bin_dir, "soroban", "27.1.0", true);
    write_unrunnable_cli(&second_dir, "stellar");

    let path = format!(
        "{}:{}",
        bin_dir.to_string_lossy(),
        second_dir.to_string_lossy()
    );

    doctor(&sandbox, &path, &data_home)
        .assert()
        .success()
        // The two that answered agree, and that is the most useful thing known
        // here -- the executable that could not be asked leaves it standing
        // rather than wiping it out.
        .stderr(contains(
            "every one that answered reports 27.1.0, but 1 could not be asked",
        ))
        .stderr(contains("different versions").not());
}

#[test]
fn confirms_the_cache_writer_when_it_is_this_cli() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    let data_home = seed_cache_writer(
        &sandbox,
        serde_json::json!({ "version": pkg(), "executable": running_binary() }),
    );

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(format!(
            "Version cache last checked by: {} ({})",
            pkg(),
            running_binary()
        )));
}

#[test]
fn does_not_warn_when_the_same_install_wrote_the_cache_at_an_older_version() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    // An in-place upgrade: same path, earlier version recorded against it.
    let data_home = seed_cache_writer(
        &sandbox,
        serde_json::json!({ "version": "26.1.0", "executable": running_binary() }),
    );

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(format!(
            "Version cache was last checked by this install running 26.1.0; it is now {}",
            pkg()
        )))
        .stderr(contains("a different Stellar CLI").not());
}

#[test]
fn warns_when_a_different_install_wrote_the_cache() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    let data_home = seed_cache_writer(
        &sandbox,
        serde_json::json!({ "version": "22.8.0", "executable": "/opt/elsewhere/bin/soroban" }),
    );

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Version cache was last checked by a different Stellar CLI: \
             22.8.0 (/opt/elsewhere/bin/soroban)",
        ))
        .stderr(contains(format!("this one is {} (", pkg())));
}

#[test]
fn does_not_warn_when_the_other_binary_name_wrote_the_cache_at_this_version() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    // One install ships both `stellar` and `soroban`, so the old name beside
    // this one is the ordinary state for anyone who still invokes it. It
    // recorded the version this CLI would have recorded itself, so there is
    // nothing here to disagree with `stellar --version`.
    let sibling = sibling_binary("soroban");
    let data_home = seed_cache_writer(
        &sandbox,
        serde_json::json!({ "version": pkg(), "executable": sibling }),
    );

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(format!(
            "Version cache was last checked by a different Stellar CLI at the same version: \
             {} ({sibling})",
            pkg()
        )))
        // Names this CLI, version included: the shared version is the reason
        // this is not a warning, so losing it here would drop the fact that
        // makes the line true.
        .stderr(contains(format!("this one is {} (", pkg())))
        // The warning keeps its colon straight after "CLI", so this misses the
        // line above and catches only the warning itself.
        .stderr(contains("a different Stellar CLI:").not());
}

#[test]
fn warns_when_the_other_binary_name_wrote_the_cache_at_another_version() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    // Sitting beside this executable is not proof of a shared install: a
    // `soroban` left behind by `cargo install soroban-cli` shares `~/.cargo/bin`
    // with the `stellar` that replaced it, years apart in version. That is the
    // confusion #2464 is about, so folding the two names into one identity
    // would silence this report exactly where it is needed.
    let sibling = sibling_binary("soroban");
    let data_home = seed_cache_writer(
        &sandbox,
        serde_json::json!({ "version": "22.8.0", "executable": sibling }),
    );

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(format!(
            "Version cache was last checked by a different Stellar CLI: 22.8.0 ({sibling})"
        )))
        .stderr(contains("at the same version").not());
}

#[test]
fn reports_an_unknown_cache_writer_without_claiming_a_mismatch() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    // A cache written before the writer was recorded.
    let data_home = seed_cache_writer(&sandbox, serde_json::Value::Null);

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Version cache was last checked by an unknown Stellar CLI",
        ))
        .stderr(contains("a different Stellar CLI").not());
}

/// Tokio's `output()` pipes stdout and stderr but leaves stdin untouched, so a
/// probe inherits whatever the shell handed `doctor` -- `std`'s `output()`,
/// which this probe used before it moved onto Tokio, closes it instead. Any
/// executable on `PATH` named `stellar`/`soroban` is run here, so one that
/// reads stdin would swallow input meant for the shell.
///
/// Deterministic rather than timed: the fake CLI writes what it saw, so the
/// unfixed behavior shows up as `read:<the sentinel>` rather than as a probe
/// that merely took longer.
#[test]
fn does_not_hand_a_probe_the_callers_stdin() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "stdin-reading-install");
    let data_home = empty_dir(&sandbox, "data-home");
    let stdin_log = sandbox.dir().join("stdin-reading-install.log");
    write_stdin_reading_cli(&bin_dir, "stellar", "27.1.0", &stdin_log);

    doctor(&sandbox, &bin_dir, &data_home)
        .write_stdin("SENTINEL\n")
        .assert()
        .success();

    let seen = fs::read_to_string(&stdin_log).unwrap();
    assert!(!seen.is_empty(), "the probe never ran");
    assert!(
        !seen.contains("SENTINEL"),
        "a probe read the caller's stdin: {seen}"
    );
}

/// Tokio's `Command` does not kill a child on drop by default. `doctor` probes
/// each executable on `PATH` under a timeout, and dropping a timed-out probe's
/// `output()` future without `kill_on_drop(true)` would leave that child
/// running for the rest of its natural life -- here, forever, since the fake
/// CLI loops indefinitely -- rather than being killed when the probe gives up
/// on it. This test fails against that unfixed behavior: the hanging script's
/// PID would still be alive long after `doctor` exits.
#[test]
fn does_not_leave_an_orphaned_process_when_a_probe_times_out() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "hanging-install");
    let data_home = empty_dir(&sandbox, "data-home");
    let pid_file = sandbox.dir().join("hanging-install.pids");
    write_hanging_cli(&bin_dir, "stellar", &pid_file);

    doctor(&sandbox, &bin_dir, &data_home).assert().success();

    let pids: Vec<u32> = fs::read_to_string(&pid_file)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.parse().unwrap())
        .collect();
    assert!(!pids.is_empty());

    // Read the verdict before acting on it, then clean up whatever is left
    // regardless of what it says. Failing here means the probes leaked, and
    // what they leaked is a shell spinning on a full core with no exit
    // condition -- asserting first would strand one per failing run.
    let survivors: Vec<u32> = pids
        .iter()
        .copied()
        .filter(|pid| process_is_alive(*pid))
        .collect();

    for pid in &pids {
        kill(*pid, "-9");
    }

    assert!(
        survivors.is_empty(),
        "processes {survivors:?} were still running after doctor exited"
    );
}

/// Whether `pid` still refers to a live process, checked with `kill -0`
/// rather than anything Tokio-specific so this holds regardless of how the
/// probe was implemented.
///
/// Polls briefly instead of checking once: the child is killed on drop, but
/// getting reaped by Tokio's orphan queue happens asynchronously, so a single
/// check right after `doctor` exits could race the reap and see a zombie that
/// is gone a moment later.
fn process_is_alive(pid: u32) -> bool {
    for attempt in 0..20 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if !kill(pid, "-0") {
            return false;
        }
    }

    // Still answering to `kill -0` after ~1s of polling: not a reap race,
    // actually still running.
    true
}

/// Send `signal` to `pid`, reporting whether `kill` accepted it -- `-0` sends
/// nothing and so answers whether the process exists at all.
///
/// A pid that is already gone is an expected outcome at both call sites, so
/// `kill` reports it through its exit status alone: its complaint on stderr is
/// localized, and would land in the test output as noise.
fn kill(pid: u32, signal: &str) -> bool {
    std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}
