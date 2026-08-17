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

/// A CLI on `PATH` that starts a background process of its own and then hangs.
///
/// `kill_on_drop` reaches the probe and nothing it started, so this is what
/// distinguishes killing the process from killing its process group: the child
/// keeps running after its parent dies unless the group was signalled. Records
/// the parent's PID to `pid_file` and the child's to `child_pid_file`.
///
/// The child is started through an absolute `/bin/sh` because `doctor` narrows
/// `PATH` to the directory under test, so no bare command name would resolve.
/// The parent records `$!` itself rather than letting the child report its own
/// PID, which would race the parent's hang.
fn write_hanging_cli_with_child(dir: &Path, name: &str, pid_file: &Path, child_pid_file: &Path) {
    let script = format!(
        "#!/bin/sh\n\
         echo \"$$\" >> \"{parent}\"\n\
         /bin/sh -c 'while :; do :; done' &\n\
         echo \"$!\" >> \"{child}\"\n\
         while :; do :; done\n",
        parent = pid_file.to_string_lossy(),
        child = child_pid_file.to_string_lossy()
    );

    let path = dir.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A CLI on `PATH` that writes to stdout without stopping.
///
/// Nothing it prints is a version, and it never exits on its own, so an
/// unbounded read would buffer until memory ran out. The line is a literal
/// rather than built with `seq`/`printf`: `doctor` narrows `PATH` to the
/// directory under test, so no external command would resolve here.
fn write_flooding_cli(dir: &Path, name: &str) {
    let line = "x".repeat(1024);
    let path = dir.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\nwhile :; do echo \"{line}\"; done\n"),
    )
    .unwrap();
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
/// `latest_check_time` is irrelevant to what is asserted here: `doctor` runs its
/// check with `CachePolicy::ReadOnly`, so it always attempts a refresh and never
/// writes the result. The seeded writer is what gets reported however many times
/// `doctor` runs -- see `reports_the_same_cache_writer_on_a_second_run`.
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
    write_fake_cli(&bin_dir, "stellar", pkg(), true);

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
        //
        // Matched as a prefix, without the closing parenthesis: the listing may
        // append a currency verdict, and whether it does depends on whether a
        // latest release was known -- which depends on the network. What this
        // test is about is the path and the version being listed at all;
        // `marks_which_installs_on_path_are_behind_the_latest_release` owns the
        // verdict.
        .stderr(contains(format!(
            "- {} ({}",
            bin_dir.join("stellar").to_string_lossy(),
            pkg()
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
    write_fake_cli(&bin_dir, "stellar", pkg(), true);
    // Old enough to only answer `--version`, which is the fallback path.
    write_fake_cli(&bin_dir, "soroban", "22.8.0", false);

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Found 2 Stellar CLI executables on PATH reporting different versions",
        ))
        // Prefix matches: an outdated entry gains a currency verdict, and
        // whether one is known depends on the network. See the lone-install test
        // above.
        .stderr(contains(format!(
            "- {} ({}",
            bin_dir.join("stellar").to_string_lossy(),
            pkg()
        )))
        .stderr(contains(format!(
            "- {} (22.8.0",
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
fn reports_a_cache_that_predates_the_writer_field_without_claiming_a_mismatch() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    // A cache written before the writer was recorded.
    let data_home = seed_cache_writer(&sandbox, serde_json::Value::Null);

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Version cache predates the record of which Stellar CLI writes it",
        ))
        .stderr(contains("a different Stellar CLI").not());
}
/// The ordinary state of a machine that has never run the CLI before, and the
/// state a user reaches by deleting the data home. Reporting it as a writer --
/// even an unknown one -- would name a Stellar CLI that never ran, which is the
/// wrong thing to tell someone who came to `doctor` suspecting a rogue install.
#[test]
fn does_not_invent_a_cache_writer_when_there_is_no_cache() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    let data_home = empty_dir(&sandbox, "empty-data-home");

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains("No version cache yet"))
        .stderr(contains("last checked by").not());
}
/// A present-but-unreadable cache is the one state here that is a fault: the
/// next check cannot be paced off it and no warning built from it can be
/// trusted. It must not read like the benign cases above.
#[test]
fn warns_when_the_cache_cannot_be_read() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    let data_home = empty_dir(&sandbox, "corrupt-data-home");
    fs::write(data_home.join("upgrade_check.json"), "{ truncated").unwrap();

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains("Version cache exists but could not be read"))
        .stderr(contains("No version cache yet").not())
        .stderr(contains("predates the record").not());
}
/// `doctor` reports which install last wrote the version cache, so it must not
/// become that install itself. Its own check runs read-only for exactly this
/// reason: otherwise the first run overwrites the entry it just reported, and a
/// user who runs `doctor` again -- to show a colleague, or to paste into an issue
/// -- finds the mismatch gone and cannot reproduce what they saw.
#[test]
fn reports_the_same_cache_writer_on_a_second_run() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "bin");
    let data_home = seed_cache_writer(
        &sandbox,
        serde_json::json!({ "version": "22.8.0", "executable": "/opt/elsewhere/bin/soroban" }),
    );

    let expected = contains(
        "Version cache was last checked by a different Stellar CLI: \
         22.8.0 (/opt/elsewhere/bin/soroban)",
    );

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(expected.clone());

    // The same report, not a report of the run that just happened.
    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(expected);
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

/// `kill_on_drop` signals the probe and nothing the probe started. A wrapper
/// script on `PATH` -- a shim, a version manager, anything that execs something
/// else in the background -- outlives the probe through its children unless the
/// whole process group is killed, which is why the probe is given a group of its
/// own. Without that, the child here keeps spinning after `doctor` exits.
#[test]
fn kills_what_a_timed_out_probe_started_too() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "hanging-install-with-child");
    let data_home = empty_dir(&sandbox, "data-home");
    let pid_file = sandbox.dir().join("parent.pids");
    let child_pid_file = sandbox.dir().join("child.pids");
    write_hanging_cli_with_child(&bin_dir, "stellar", &pid_file, &child_pid_file);

    doctor(&sandbox, &bin_dir, &data_home).assert().success();

    let children: Vec<u32> = fs::read_to_string(&child_pid_file)
        .unwrap()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();
    assert!(!children.is_empty(), "the fake CLI started no child");

    let survivors = survivors_of(&children);

    kill_all_in(&pid_file);
    kill_all_in(&child_pid_file);

    assert!(
        survivors.is_empty(),
        "a timed-out probe's children {survivors:?} outlived it"
    );
}
/// Each hung executable costs a full probe timeout, so probing in series makes
/// the wait grow with the number of executables on `PATH` -- which is exactly
/// what the per-probe timeout exists to prevent, defeated by arithmetic. With
/// eight hung executables, serial probing cannot finish inside this bound and
/// concurrent probing cannot exceed it: one timeout plus `doctor`'s own work.
#[test]
fn probes_installs_concurrently_so_hung_ones_do_not_accumulate() {
    let sandbox = TestEnv::default();
    let data_home = empty_dir(&sandbox, "data-home");
    let pid_file = sandbox.dir().join("many-hanging.pids");

    // Two binary names per directory, so four directories put eight hung
    // executables on `PATH`.
    let dirs: Vec<PathBuf> = (0..4)
        .map(|i| {
            let dir = empty_dir(&sandbox, &format!("many-hanging-{i}"));
            write_hanging_cli(&dir, "stellar", &pid_file);
            write_hanging_cli(&dir, "soroban", &pid_file);
            dir
        })
        .collect();

    let path = dirs
        .iter()
        .map(|dir| dir.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");

    let started = std::time::Instant::now();
    let assert = doctor(&sandbox, &path, &data_home).assert().success();
    let elapsed = started.elapsed();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    kill_all_in(&pid_file);

    assert!(
        stderr.contains("8 executables on PATH did not answer within 2s"),
        "expected all eight to be probed and reported hung: {stderr}"
    );
    // Serial probing spends 8 x 2s here. The bound leaves generous room for
    // `doctor`'s crates.io fetch, which has a 5s timeout of its own.
    assert!(
        elapsed < std::time::Duration::from_secs(14),
        "probing eight hung executables took {elapsed:?}, which is serial, not concurrent"
    );
}
/// An empty or relative `PATH` entry resolves against the working directory, so
/// a file named `stellar` in whatever directory the user happens to be in would
/// be executed by a probe. `doctor` runs every match it finds and must not be
/// reachable that way; the marker file is what a run would leave behind.
#[test]
fn does_not_probe_an_executable_reached_through_a_relative_path_entry() {
    let sandbox = TestEnv::default();
    let cwd = empty_dir(&sandbox, "working-directory");
    let relative_bin = empty_dir(&sandbox, "working-directory/relative-bin");
    let data_home = empty_dir(&sandbox, "data-home");
    let marker = sandbox.dir().join("relative-probe-ran");

    for dir in [&cwd, &relative_bin] {
        let path = dir.join("stellar");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho ran >> \"{}\"\necho \"22.8.0\"\n",
                marker.to_string_lossy()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // An empty entry and a relative one, which is every way `PATH` can name the
    // working directory.
    let mut cmd = doctor(&sandbox, ":relative-bin", &data_home);
    cmd.current_dir(&cwd).assert().success();

    assert!(
        !marker.exists(),
        "a probe ran an executable found through a relative PATH entry"
    );
}
/// An executable that writes without stopping and never exits would be buffered
/// in full by an unbounded read. The probe caps what it will hold and treats
/// reaching the cap as the answer -- nothing that long is a version banner -- so
/// this finishes promptly and reports the executable as unreadable rather than
/// growing until memory runs out.
#[test]
fn does_not_buffer_unbounded_output_from_a_probe() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "flooding-install");
    let data_home = empty_dir(&sandbox, "data-home");
    write_flooding_cli(&bin_dir, "stellar");

    let started = std::time::Instant::now();
    let assert = doctor(&sandbox, &bin_dir, &data_home).assert().success();
    let elapsed = started.elapsed();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    assert!(
        stderr.contains(&format!(
            "- {} (unknown version)",
            bin_dir.join("stellar").to_string_lossy()
        )),
        "a flooding executable should be reported as unreadable, not hung: {stderr}"
    );
    // The cap is reached in well under a millisecond of the child's output, so
    // neither probe should reach its timeout. Two timeouts would be 4s.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "reading a flooding executable took {elapsed:?}; the cap did not cut it short"
    );
}
/// The version confusion in #2464 does not need the `PATH` set to disagree with
/// itself: one stale `stellar` on `PATH`, beside a current binary invoked by
/// absolute path, is an internally consistent set of one and still exactly the
/// contradiction the user is looking at. A success tick over that is the wrong
/// signal, so agreement is measured against the running CLI too.
#[test]
fn does_not_bless_a_lone_install_that_is_not_the_version_running() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "stale-lone-install");
    let data_home = empty_dir(&sandbox, "data-home");
    // Above every published release, so it differs from the running CLI without
    // being outdated. That is the case this check uniquely catches -- and it is
    // what makes the assertion deterministic: an install that were merely old
    // would be absorbed by the "behind the latest release" line instead, and
    // whether a latest release is known at all depends on the network.
    write_fake_cli(&bin_dir, "stellar", "999.0.0", false);

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(
            "Only one Stellar CLI on PATH, and it is not the version running",
        ))
        .stderr(contains(format!(
            "reports a version other than the {} now running, without being behind the latest \
             release",
            pkg()
        )))
        .stderr(contains("Only one Stellar CLI found on PATH").not());
}
/// The manual step #2464 is a report of: the reporter saw two version numbers,
/// could not tell which install each belonged to or which one was current, and
/// opened an issue. Listing the installs is half the answer; saying which of
/// them is behind the latest release is the half that removes the comparison.
///
/// Deterministic with or without a network, which matters because `doctor`
/// always attempts a crates.io fetch and falls back to the cache. The two fake
/// versions bracket every value the latest release could take: `1.0.0` is below
/// any published release, `999.0.0` is above any of them, and the seeded cache
/// guarantees a fallback so an offline run still knows a latest version at all.
/// So the verdict on each is fixed even though the yardstick is not.
#[test]
fn marks_which_installs_on_path_are_behind_the_latest_release() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "outdated-and-ahead");
    let data_home = seed_cache_writer(&sandbox, serde_json::Value::Null);
    write_fake_cli(&bin_dir, "stellar", "1.0.0", true);
    write_fake_cli(&bin_dir, "soroban", "999.0.0", true);

    let assert = doctor(&sandbox, &bin_dir, &data_home).assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    let outdated = format!(
        "- {} (1.0.0, outdated -- ",
        bin_dir.join("stellar").display()
    );
    assert!(
        stderr.contains(&outdated),
        "the outdated install should be marked in the listing: {stderr}"
    );

    // The one ahead of every release is listed plainly. A verdict here would be
    // invented: nothing establishes that a version above the latest is wrong.
    let ahead = format!("- {} (999.0.0)", bin_dir.join("soroban").display());
    assert!(
        stderr.contains(&ahead),
        "the install ahead of the latest release should carry no verdict: {stderr}"
    );

    // The actionable sentence, and the count that makes it actionable: one of
    // the two, not both, and not "some".
    assert!(
        stderr.contains("1 executable on PATH is behind the latest release"),
        "the count of outdated installs should be stated: {stderr}"
    );

    // The two lines divide the installs rather than both claiming the outdated
    // one: being behind the latest release already explains why it differs from
    // the CLI in use, so only the other is left for the running-version line.
    assert!(
        stderr.contains(&format!(
            "1 executable on PATH reports a version other than the {} now running, without \
             being behind the latest release",
            pkg()
        )),
        "the running-version line should cover only what being outdated does not: {stderr}"
    );
}
/// An executable that could not be asked for its version has no version to
/// compare, so it must be listed without a currency verdict -- neither outdated
/// nor current. The reason it went unanswered survives into the label unchanged.
#[test]
fn does_not_judge_the_currency_of_an_executable_that_did_not_answer() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "unreadable-currency");
    let data_home = seed_cache_writer(&sandbox, serde_json::Value::Null);
    write_unrunnable_cli(&bin_dir, "stellar");

    doctor(&sandbox, &bin_dir, &data_home)
        .assert()
        .success()
        .stderr(contains(format!(
            "- {} (unknown version)",
            bin_dir.join("stellar").display()
        )))
        .stderr(contains("outdated").not())
        .stderr(contains("behind the latest release").not());
}
/// A hung executable and an unreadable one are both "did not answer", but only
/// the first is itself the fault -- and only the first explains why `doctor` took
/// as long as it did. Issue #2676 asked for that distinction to be decided
/// rather than left implicit; this is the decision, asserted.
#[test]
fn distinguishes_a_hung_executable_from_one_that_cannot_answer() {
    let sandbox = TestEnv::default();
    let bin_dir = empty_dir(&sandbox, "hung-and-unreadable");
    let data_home = empty_dir(&sandbox, "data-home");
    let pid_file = sandbox.dir().join("hung-and-unreadable.pids");
    write_hanging_cli(&bin_dir, "stellar", &pid_file);
    write_unrunnable_cli(&bin_dir, "soroban");

    let assert = doctor(&sandbox, &bin_dir, &data_home).assert().success();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();

    // Clean up before asserting: a failing assertion must not be what stops
    // the cleanup from running.
    for pid in fs::read_to_string(&pid_file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.parse::<u32>().ok())
    {
        kill(pid, "-9");
    }

    assert!(
        stderr.contains("did not answer within 2s and had to be killed"),
        "the hung executable was not called out: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "- {} (hung; killed after timing out)",
            bin_dir.join("stellar").to_string_lossy()
        )),
        "the hung executable was not labelled distinctly: {stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "- {} (unknown version)",
            bin_dir.join("soroban").to_string_lossy()
        )),
        "the unrunnable executable should keep the unknown-version label: {stderr}"
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

/// Whether `pid` still refers to a live process of its own.
///
/// Signal 0 alone is not enough: it succeeds for a zombie, which is a process
/// that has already died and is only waiting to be reaped. The child is killed
/// when the timed-out probe is dropped, but reaping happens asynchronously --
/// through Tokio's orphan queue, or through init once `doctor` exits -- so a
/// zombie right after `doctor` returns is the expected outcome of a *correct*
/// kill, not evidence of a leak. Polling past it is what the previous version of
/// this check did, and it made the test tolerate a real leak for a second before
/// noticing. Reading the state instead answers the question directly.
fn process_is_alive(pid: u32) -> bool {
    for attempt in 0..20 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if !kill(pid, "-0") || process_is_a_zombie(pid) {
            return false;
        }
    }

    // Still a live, non-zombie process after ~1s: not a reap race, actually
    // still running.
    true
}

/// Whether `pid` has exited and is only awaiting a reap.
///
/// Asked through `ps` rather than `/proc`, which macOS does not have. `ps` is
/// resolved through the environment's `PATH` like any other command, and an
/// environment without it degrades to "not a zombie" rather than failing: the
/// caller's liveness check is the load-bearing half, and this only refines it.
fn process_is_a_zombie(pid: u32) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .starts_with('Z')
}
/// Kill every PID listed in `pid_file`, ignoring ones already gone.
///
/// Called before asserting, in every test that starts a hanging fake, so that a
/// failing assertion is never what stops the cleanup from running.
fn kill_all_in(pid_file: &Path) -> Vec<u32> {
    let pids: Vec<u32> = fs::read_to_string(pid_file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();

    for pid in &pids {
        kill(*pid, "-9");
    }

    pids
}
/// Wait briefly for every PID in `pids` to be gone, then report the survivors.
///
/// Polls rather than checking once: the kill is delivered as `doctor` exits, and
/// the reap that follows it is asynchronous.
fn survivors_of(pids: &[u32]) -> Vec<u32> {
    for attempt in 0..20 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        if pids.iter().all(|pid| !process_is_alive_once(*pid)) {
            return Vec::new();
        }
    }

    pids.iter()
        .copied()
        .filter(|pid| process_is_alive_once(*pid))
        .collect()
}
/// One check, no polling: alive and not merely awaiting a reap.
fn process_is_alive_once(pid: u32) -> bool {
    kill(pid, "-0") && !process_is_a_zombie(pid)
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
