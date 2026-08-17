use clap::Parser;
use rustc_version::version;
use semver::Version;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{
    commands::{container::shared::Engine, global},
    config::{
        self, data,
        locator::{self, KeyType},
        network::{Network, DEFAULTS as DEFAULT_NETWORKS},
        upgrade_check::{LastWriter, UpgradeCheck},
    },
    print::Print,
    rpc,
    upgrade_check::{
        check_performed_by, has_available_upgrade, running_binary, upgrade_message, CachePolicy,
    },
    utils::url::redact_url,
};

#[derive(Parser, Debug, Clone)]
#[group(skip)]
pub struct Cmd {
    #[command(flatten)]
    pub config_locator: locator::Args,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Locator(#[from] locator::Error),

    #[error(transparent)]
    Network(#[from] config::network::Error),

    #[error(transparent)]
    RpcClient(#[from] rpc::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Data(#[from] data::Error),
}

impl Cmd {
    pub async fn run(&self, _global_args: &global::Args) -> Result<(), Error> {
        let print = Print::new(false);

        // `doctor` reports on the cache, so it must not become the cache's last
        // writer: its own check runs `ReadOnly` and leaves the file alone. The
        // read is still taken first, which keeps the report the same whichever
        // order these run in, and `cli::runs_its_own_upgrade_check` keeps the
        // background check from being spawned alongside this command -- that one
        // does write. Between the two, running `doctor` twice reports the same
        // writer both times instead of erasing it on the first run.
        let previous_cache_writer = UpgradeCheck::last_writer();

        let latest_release = check_version(&print).await?;
        check_installs(&print, latest_release.as_ref()).await;
        show_version_cache_writer(&print, &previous_cache_writer);
        check_rust_version(&print);
        check_wasm_target(&print);
        check_optional_features(&print);
        check_container_engine(&print);
        show_config_path(&print, &self.config_locator)?;
        show_data_path(&print)?;
        show_xdr_version(&print);
        inspect_networks(&print, &self.config_locator).await?;

        Ok(())
    }
}

fn show_config_path(print: &Print, config_locator: &locator::Args) -> Result<(), Error> {
    let global_path = config_locator.global_config_path()?;

    print.gearln(format!(
        "Config directory: {}",
        global_path.to_string_lossy()
    ));

    Ok(())
}

fn show_data_path(print: &Print) -> Result<(), Error> {
    let path = data::data_local_dir()?;

    print.dirln(format!("Data directory: {}", path.to_string_lossy()));

    Ok(())
}

fn show_xdr_version(print: &Print) {
    let xdr = stellar_xdr::VERSION;

    print.infoln(format!("XDR version: {}", xdr.xdr));
}

async fn print_network(
    default: bool,
    print: &Print,
    name: &str,
    network: &Network,
) -> Result<(), Error> {
    let client = network.rpc_client()?;
    let version_info = client.get_version_info().await?;

    let prefix = if default {
        "Default network"
    } else {
        "Network"
    };

    print.globeln(format!(
        "{prefix} {name:?} ({})",
        redact_url(&network.rpc_url)
    ));
    print.blankln(format!("protocol {}", version_info.protocol_version));
    print.blankln(format!("rpc {}", version_info.version));

    Ok(())
}

async fn inspect_networks(print: &Print, config_locator: &locator::Args) -> Result<(), Error> {
    let saved_networks = KeyType::Network.list_paths(&config_locator.local_and_global()?)?;
    let default_networks = DEFAULT_NETWORKS
        .into_iter()
        .map(|(name, network)| ((*name).to_string(), network.into()));

    for (name, network) in default_networks {
        // Skip default mainnet, because it has no default rpc url.
        if name == "mainnet" {
            continue;
        }

        if print_network(true, print, &name, &network).await.is_err() {
            print.warnln(format!(
                "Default network {name:?} ({}) is unreachable",
                redact_url(&network.rpc_url)
            ));
        }
    }

    for (name, _) in &saved_networks {
        if let Ok(network) = config_locator.read_network(name) {
            if print_network(false, print, name, &network).await.is_err() {
                print.warnln(format!(
                    "Network {name:?} ({}) is unreachable",
                    redact_url(&network.rpc_url)
                ));
            }
        }
    }

    Ok(())
}

/// Report whether this CLI is current, and return the latest release it was
/// judged against so the installs found on `PATH` can be judged against the
/// same one.
///
/// Returns `None` when no latest version is known. The empty cache holds
/// `0.0.0`, so an offline run with nothing to fall back on would otherwise
/// "learn" that every install on the machine is ahead of the latest release.
async fn check_version(print: &Print) -> Result<Option<Version>, Error> {
    let Ok((upgrade_available, current_version, latest_version)) =
        has_available_upgrade(CachePolicy::ReadOnly).await
    else {
        return Ok(None);
    };

    if upgrade_available {
        print.warnln(upgrade_message(&current_version, &latest_version));
    } else {
        print.checkln(format!(
            "You are using the latest version of Stellar CLI: {current_version}"
        ));
    }

    Ok((latest_version > Version::new(0, 0, 0)).then_some(latest_version))
}

/// Report which CLI last checked for a new release and wrote the shared version
/// cache.
///
/// Every install writes the same file, so the entry that decides when the next
/// check happens -- and the versions any warning is built from -- may have come
/// from a different install than the one being run. When it did, say so plainly:
/// that mismatch is the signal worth surfacing.
///
/// "Checked", not "refreshed": a check whose fetch failed still stamps the file
/// while leaving the recorded versions untouched, so the writer names the
/// install that paced the next check and cannot be said to have supplied the
/// versions stored beside it.
///
/// Identity is the executable path, not the recorded version. An in-place
/// upgrade leaves an older version recorded against the very path now running,
/// and that is one install, not two -- treating it as a mismatch would warn
/// about every upgrade until the cache was next written.
///
/// The version then decides how loudly a difference in path is reported, not
/// whether there is one. An executable that recorded this very version has
/// nothing to mislead anyone with, whichever install it belongs to, so it is
/// reported rather than warned about. The warning is kept for the case that
/// actually confuses people: a different path *and* a different version.
///
/// Folding paths together instead -- reading `stellar` and `soroban` beside
/// each other as one install, since one install does ship both -- would silence
/// that case exactly where it is most common. A `soroban` left behind by
/// `cargo install soroban-cli` sits in the same `~/.cargo/bin` as the `stellar`
/// that replaced it, at a version years apart; siblings are the shape of the
/// confusion in #2464, not proof of a shared origin.
fn show_version_cache_writer(print: &Print, last_writer: &LastWriter) {
    let writer = match last_writer {
        // Ordinary, and true of every first run on a machine. Saying an unknown
        // CLI wrote the file would invent a third party for a user who is
        // already suspicious of one.
        LastWriter::NoCache => {
            print.checkln(
                "No version cache yet; nothing has checked for a new release on this machine"
                    .to_string(),
            );
            return;
        }
        // A real fault, and the one state here that is worth acting on: the file
        // is present and cannot be read, so the next check cannot be paced and
        // no warning built from it can be trusted.
        LastWriter::Unreadable => {
            print.warnln(
                "Version cache exists but could not be read; delete it to let the next check \
                 rewrite it"
                    .to_string(),
            );
            return;
        }
        // The cache predates the field. Nothing is wrong; there is simply
        // nothing recorded to compare against.
        LastWriter::Unrecorded => {
            print.infoln(
                "Version cache predates the record of which Stellar CLI writes it, so its writer \
                 is unknown"
                    .to_string(),
            );
            return;
        }
        LastWriter::Recorded(writer) => writer,
    };

    let this_cli = check_performed_by();

    match (&writer.executable, &this_cli.executable) {
        (Some(written_by), Some(running)) if written_by == running => {
            if writer.version == this_cli.version {
                print.checkln(format!("Version cache last checked by: {writer}"));
            } else {
                // Same install, earlier version: the ordinary state after an
                // upgrade, and it corrects itself at the next check.
                let recorded = writer.version.as_deref().unwrap_or("an unknown version");
                let running = this_cli.version.as_deref().unwrap_or("unknown");

                print.infoln(format!(
                    "Version cache was last checked by this install running {recorded}; \
                     it is now {running}"
                ));
            }
        }
        // A different file, but not a different answer: it recorded the version
        // this CLI would have recorded itself, so nothing taken from the cache
        // can disagree with `stellar --version`. One install ships both
        // `stellar` and `soroban`, so this is the ordinary state for anyone who
        // still invokes the old name -- and two installs that happen to sit at
        // one version have nothing to tell apart either.
        (Some(_), Some(_)) if writer.version.is_some() && writer.version == this_cli.version => {
            print.infoln(format!(
                "Version cache was last checked by a different Stellar CLI at the same version: \
                 {writer}"
            ));
            print.blankln(format!("this one is {this_cli}"));
        }
        (Some(_), Some(_)) => {
            print.warnln(format!(
                "Version cache was last checked by a different Stellar CLI: {writer}"
            ));
            print.blankln(format!("this one is {this_cli}"));
        }
        // Without both paths there is no identity to compare, so report the
        // writer without claiming it was or was not this install.
        _ => print.infoln(format!("Version cache was last checked by: {writer}")),
    }
}

/// The binary names this CLI ships under. Both are built from the same crate,
/// so a stale `soroban` runs the same upgrade check as a current `stellar` and
/// reports its own, older version.
const CLI_BINARY_NAMES: [&str; 2] = ["stellar", "soroban"];

/// Report the running executable and every other Stellar CLI on `PATH`.
///
/// Several executables at *different versions* is the case that makes the
/// upgrade warning look wrong: an old binary correctly reports its own old
/// version, but the user compares it against whichever binary
/// `stellar --version` resolves to and sees a contradiction. Listing them makes
/// that visible.
///
/// Count alone is not the signal. This crate ships two binaries, `stellar` and
/// `soroban`, so a single healthy install puts two files on `PATH` -- warning
/// about that would flag nearly every install. Warn when the versions actually
/// disagree.
async fn check_installs(print: &Print, latest: Option<&Version>) {
    match running_binary() {
        Some(binary) => print.infoln(format!("Running executable: {binary}")),
        None => print.warnln("Could not determine the running executable".to_string()),
    }

    let installs = find_installs().await;
    let this_version = crate::commands::version::pkg();

    // Checked against the running binary, not only between the executables
    // found. An install on `PATH` that answers with a version this binary does
    // not share is the whole shape of #2464 -- and the `PATH` set need not
    // disagree with *itself* for that to happen. A lone stale `stellar` on
    // `PATH`, next to a current binary the user invoked by absolute path, is an
    // internally consistent set of one and still the exact confusion being
    // diagnosed. Without this, that case earns a success tick.
    //
    // Reported below only for the executables that being *behind the latest
    // release* does not already account for. In the ordinary case the two
    // overlap completely -- a forgotten old `soroban` is both -- and saying it
    // twice buries the one line that carries an action. What is left is the case
    // this check uniquely catches: an install that differs from the running one
    // without being out of date at all.
    let stale: Vec<&PathBuf> = installs
        .iter()
        .filter(|(_, version)| version.known().is_some_and(|found| found != this_version))
        .filter(|(_, version)| version.is_behind(latest) != Some(true))
        .map(|(path, _)| path)
        .collect();

    // The success tick, though, turns on any disagreement with the running CLI,
    // including one already explained by being outdated. An install behind the
    // latest release is still not the CLI in use.
    let agrees_with_this_cli = !installs
        .iter()
        .any(|(_, version)| version.known().is_some_and(|found| found != this_version));

    let hung: Vec<&PathBuf> = installs
        .iter()
        .filter(|(_, version)| *version == InstallVersion::TimedOut)
        .map(|(path, _)| path)
        .collect();

    summarize_installs(print, &installs, agrees_with_this_cli, this_version);
    list_installs(print, &installs, latest);

    report_install_currency(print, &installs, latest, &stale, &hung, this_version);
}

/// The one line that says what the discovered executables amount to.
///
/// Split out of `check_installs` because it answers one question -- what the set
/// establishes about itself -- across a wide match, while its caller answers a
/// different one: what the reader should do about it.
fn summarize_installs(
    print: &Print,
    installs: &[Install],
    agrees_with_this_cli: bool,
    this_version: &str,
) {
    match (installs.len(), summarize_versions(installs)) {
        (0, _) => print.warnln(
            "No Stellar CLI found on PATH; the running executable is not reachable by name"
                .to_string(),
        ),
        // One file, so nothing can disagree with it. Still list it: the running
        // executable is not necessarily the one `PATH` resolves by name, and the
        // line above carries no version.
        (1, InstalledVersions::Agreed(_)) if agrees_with_this_cli => {
            print.checkln("Only one Stellar CLI found on PATH:".to_string());
        }
        (1, InstalledVersions::Agreed(_)) => print
            .warnln("Only one Stellar CLI on PATH, and it is not the version running:".to_string()),
        // Nothing disagreed here either, but nothing answered: a success tick
        // above a line reading "unknown version" contradicts itself, and the
        // very same failed probe warns as soon as a second executable exists.
        // A `stellar` on `PATH` that cannot say what it is deserves the same
        // doubt whether or not it has company.
        (1, InstalledVersions::Unanswered { .. }) => print.warnln(
            "Found one Stellar CLI on PATH, but it did not report a version; it may be broken \
             or not a Stellar CLI at all:"
                .to_string(),
        ),
        (count, InstalledVersions::Agreed(version)) if agrees_with_this_cli => print.checkln(
            format!("{count} Stellar CLI executables on PATH, all reporting {version}:"),
        ),
        // Internally consistent and still not the CLI in use, so the agreement
        // is real but not reassuring. Name the version they settled on, since
        // that is what disagrees with the one running.
        (count, InstalledVersions::Agreed(version)) => print.warnln(format!(
            "{count} Stellar CLI executables on PATH, all reporting {version}, which is not the \
             {this_version} now running:"
        )),
        (count, InstalledVersions::Disagree { unanswered: 0 }) => print.warnln(format!(
            "Found {count} Stellar CLI executables on PATH reporting different versions:"
        )),
        // Only the ones that answered were seen to disagree. Folding the silent
        // ones into that count would attribute an observation to executables
        // that were never heard from.
        (count, InstalledVersions::Disagree { unanswered }) => print.warnln(format!(
            "Found {count} Stellar CLI executables on PATH; the {} that reported a version do \
             not agree ({unanswered} could not be asked):",
            count - unanswered
        )),
        // Not a version disagreement -- we could not establish one either way,
        // and saying "different versions" here would name a cause that has not
        // been observed. What the executables that did answer settled between
        // themselves still holds, so lead with it.
        (
            count,
            InstalledVersions::Unanswered {
                agreed: Some(version),
                unanswered,
            },
        ) => print.warnln(format!(
            "Found {count} Stellar CLI executables on PATH; every one that answered reports \
             {version}, but {unanswered} could not be asked, so a differing version cannot be \
             ruled out:"
        )),
        (count, InstalledVersions::Unanswered { agreed: None, .. }) => print.warnln(format!(
            "Found {count} Stellar CLI executables on PATH; none of them reported a version, \
             so whether they agree could not be determined:"
        )),
    }
}

/// What the reader should do about the executables just listed.
///
/// Three findings, each said only when it adds something the others did not: how
/// many are out of date, how many merely differ from the CLI in use, and how many
/// are hung.
fn report_install_currency(
    print: &Print,
    installs: &[Install],
    latest: Option<&Version>,
    stale: &[&PathBuf],
    hung: &[&PathBuf],
    this_version: &str,
) {
    // The listing already marks each outdated executable; this says how many
    // there are and what to do, which is the part the reader would otherwise
    // have to conclude for themselves. Nothing is said when none are behind, or
    // when no latest release is known to compare against.
    let behind = installs
        .iter()
        .filter(|(_, version)| version.is_behind(latest) == Some(true))
        .count();

    if behind > 0 {
        let latest = latest.expect("a comparison was made, so a latest version was known");

        print.warnln(format!(
            "{} on PATH {} behind the latest release ({latest}). Upgrade or remove {}: an \
             outdated one prints upgrade warnings naming its own version, which is what makes \
             them look wrong.",
            count_of(behind, "executable", "executables"),
            if behind == 1 { "is" } else { "are" },
            if behind == 1 { "it" } else { "them" },
        ));
    }

    // Said after the list, so the paths it refers to are on screen. The summary
    // line above describes what the `PATH` set says among itself; this one is
    // about the running binary, and neither substitutes for the other.
    if !stale.is_empty() {
        print.warnln(format!(
            "{} on PATH {} a version other than the {this_version} now running, without being \
             behind the latest release.",
            count_of(stale.len(), "executable", "executables"),
            if stale.len() == 1 {
                "reports"
            } else {
                "report"
            },
        ));
    }

    // A hung executable is a fault in its own right, and a different one from an
    // install that merely could not be read: it is why `doctor` took as long as
    // it did, and it will keep costing that on every run until it is dealt with.
    if !hung.is_empty() {
        print.warnln(format!(
            "{} on PATH did not answer within {}s and had to be killed. Unlike one that simply \
             failed to report a version, a hung executable is itself the problem.",
            count_of(hung.len(), "executable", "executables"),
            INSTALL_PROBE_TIMEOUT.as_secs(),
        ));
    }
}

fn count_of(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// What the discovered executables establish about which version is in play.
#[derive(Debug, PartialEq, Eq)]
enum InstalledVersions {
    /// Every executable answered, and answered the same: one version is in play
    /// however many files carry it.
    Agreed(String),
    /// Two executables reported different versions. This is the case that makes
    /// the upgrade warning look wrong. Carries how many could not be asked: they
    /// took no part in the disagreement, so they are not part of its count.
    ///
    /// Two known versions are needed to observe this, so a single executable
    /// cannot produce it however its probe went.
    Disagree { unanswered: usize },
    /// At least one executable could not be asked for its version, and none of
    /// those that did contradict each other, so agreement is unestablished
    /// rather than absent. Carries the version the ones that answered settled
    /// on, if any answered at all, and how many did not.
    Unanswered {
        agreed: Option<String>,
        unanswered: usize,
    },
}

/// Classify the reported versions, keeping "they disagree" apart from "one of
/// them could not be asked".
///
/// A disagreement between known versions is reported even when other probes
/// failed: it is an observed fact, and the failures only add to it. Either way
/// the failures are counted separately, so neither outcome speaks for an
/// executable that was never heard from.
fn summarize_versions(installs: &[Install]) -> InstalledVersions {
    let unanswered = installs
        .iter()
        .filter(|(_, version)| version.known().is_none())
        .count();

    let mut known = installs.iter().filter_map(|(_, version)| version.known());
    let first = known.next();

    if let Some(first) = first {
        if !known.all(|version| version == first) {
            return InstalledVersions::Disagree { unanswered };
        }
    }

    match (first, unanswered) {
        (Some(version), 0) => InstalledVersions::Agreed(version.to_string()),
        // Everything that answered agreed; `first` carries that version so the
        // one fact established here is not thrown away with the failed probes.
        _ => InstalledVersions::Unanswered {
            agreed: first.map(ToString::to_string),
            unanswered,
        },
    }
}

/// One discovered executable and what its probe established.
type Install = (PathBuf, InstallVersion);

/// What a probe established about one executable's version.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallVersion {
    /// It answered with a version.
    Known(String),
    /// It ran but did not answer like a Stellar CLI: it could not be spawned, it
    /// exited non-zero, or what it printed was not a version.
    Silent,
    /// It did not answer within `INSTALL_PROBE_TIMEOUT` and was killed.
    ///
    /// Kept apart from `Silent` because only this one points at an executable
    /// that is *itself* hung, which is the actionable half of the distinction:
    /// it names the `PATH` entry that cost the user the wait, and says the entry
    /// is stuck rather than merely unreadable. Both are still "did not answer"
    /// to `summarize_versions`, which reasons about agreement and has no use for
    /// the reason -- so the distinction is carried without being forced on it.
    TimedOut,
}

impl InstallVersion {
    /// The version, when one was actually read.
    fn known(&self) -> Option<&str> {
        match self {
            Self::Known(version) => Some(version),
            Self::Silent | Self::TimedOut => None,
        }
    }

    /// How the outcome reads beside the path in the listing.
    fn label(&self) -> &str {
        match self {
            Self::Known(version) => version,
            Self::Silent => "unknown version",
            Self::TimedOut => "hung; killed after timing out",
        }
    }

    /// Whether this executable is behind `latest`.
    ///
    /// `None` when the comparison cannot be made -- no latest release is known,
    /// the executable never answered, or what it answered does not parse as a
    /// version. Those are three different reasons to say nothing, and none of
    /// them is evidence that the install is current.
    fn is_behind(&self, latest: Option<&Version>) -> Option<bool> {
        let latest = latest?;
        let found = Version::parse(self.known()?).ok()?;

        Some(found < *latest)
    }
}

/// How one discovered executable reads in the listing.
///
/// The version alone leaves the reader to compare it against the latest release
/// themselves, which is the manual step #2464 is a report of: the user saw two
/// numbers, could not tell which install each belonged to or which was current,
/// and opened an issue. Saying "outdated" next to the path is the answer they
/// were assembling by hand.
///
/// Only stated when it is known. An executable that did not answer, or one
/// checked with no latest release to compare against, is listed without a
/// verdict rather than with a reassuring one.
fn install_label(version: &InstallVersion, latest: Option<&Version>) -> String {
    match version.is_behind(latest) {
        Some(true) => {
            let latest = latest.expect("a comparison was made, so a latest version was known");
            format!("{}, outdated -- {latest} is available", version.label())
        }
        Some(false) | None => version.label().to_string(),
    }
}

/// Every distinct Stellar CLI executable reachable by name on `PATH`, with the
/// version it reports.
///
/// Reported as `PATH` offers them, canonicalized only to recognize one file
/// reached two ways. A package manager that installs through a symlink -- as
/// Homebrew does -- has a canonical path carrying the version it currently
/// points at, so reporting that form names a path the user never typed and
/// cannot act on, and it disagrees with the running executable line above it
/// for what is one install.
///
/// Discovery is sequential -- it is only `PATH` lookups -- but the probes run
/// concurrently. Each costs up to two timeouts, so probing in series made the
/// worst case grow with the number of executables found: enough stalled entries
/// and `doctor` hangs for a minute, which is the thing its per-probe timeout
/// exists to prevent. Concurrently, the worst case is one executable's wait no
/// matter how many there are.
async fn find_installs() -> Vec<Install> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    for name in CLI_BINARY_NAMES {
        let Ok(found) = which::which_all(name) else {
            continue;
        };

        for path in found {
            // An empty or relative `PATH` entry resolves against the working
            // directory, so a file named `stellar` sitting in whatever directory
            // the user happens to be in would be executed by the probe below.
            // `doctor` runs every match it finds; it should not be reachable
            // that way.
            if !path.is_absolute() {
                tracing::debug!("ignoring non-absolute PATH match: {}", path.display());
                continue;
            }

            // `which_all` yields one entry per matching `PATH` element, so the
            // same binary shows up repeatedly when `PATH` has duplicates.
            let key = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            paths.push(path);
        }
    }

    let versions =
        futures::future::join_all(paths.iter().map(|path| installed_version(path))).await;

    paths.into_iter().zip(versions).collect()
}

fn list_installs(print: &Print, installs: &[Install], latest: Option<&Version>) {
    for (path, version) in installs {
        print.blankln(format!(
            "- {} ({})",
            path.to_string_lossy(),
            install_label(version, latest)
        ));
    }
}

/// Ask a CLI executable for its version.
async fn installed_version(path: &Path) -> InstallVersion {
    // `--only-version` predates neither every release nor every binary name:
    // releases old enough to cause the version confusion this check exists to
    // surface reject the flag, so fall back to parsing the full version banner.
    let only_version = run_version(path, &["version", "--only-version"]).await;

    if let ProbeOutput::Answered(output) = &only_version {
        if let Some(version) = parse_only_version(output) {
            return InstallVersion::Known(version);
        }
    }

    // An executable that hung on the first query is hung, and asking it a second
    // question only spends another timeout to learn the same thing.
    if only_version == ProbeOutput::TimedOut {
        return InstallVersion::TimedOut;
    }

    match run_version(path, &["--version"]).await {
        ProbeOutput::Answered(output) => {
            parse_version_banner(&output).map_or(InstallVersion::Silent, InstallVersion::Known)
        }
        ProbeOutput::TimedOut => InstallVersion::TimedOut,
        ProbeOutput::Failed => InstallVersion::Silent,
    }
}

/// The result of running one version query against one executable.
#[derive(Debug, PartialEq, Eq)]
enum ProbeOutput {
    Answered(String),
    /// Could not be spawned, or exited non-zero.
    Failed,
    /// Outlived `INSTALL_PROBE_TIMEOUT`.
    TimedOut,
}

// `doctor` exists to diagnose a broken install, so a broken one on `PATH` must
// not be able to hang it: something answering to `stellar`/`soroban` could be
// an unrelated, stalled, or misbehaving executable.
const INSTALL_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What a probe is allowed to make `doctor` hold in memory. A version banner is
/// a few hundred bytes; a misbehaving executable can write until it is stopped,
/// and `output()` would buffer all of it.
///
/// Reaching the cap is itself the answer. Nothing this long is a version banner,
/// so the probe stops reading and stops waiting rather than holding a partial
/// buffer against a child that may never exit -- which would otherwise turn a
/// merely verbose executable into one reported as hung.
const MAX_PROBE_OUTPUT: u64 = 64 * 1024;

async fn run_version(path: &Path, args: &[&str]) -> ProbeOutput {
    let mut command = tokio::process::Command::new(path);
    command
        .args(args)
        // Tokio does not kill a child on drop by default. Without this, a probe
        // that times out below leaves its process running -- leaking it across
        // repeated `doctor` runs against the same stalled executable.
        .kill_on_drop(true)
        // Nor does Tokio's `output()` close stdin, where `std`'s does: it pipes
        // stdout and stderr and leaves stdin alone, so the child inherits the
        // terminal. A probe is not a conversation -- anything on `PATH`
        // answering to `stellar`/`soroban` gets run here, and one that reads
        // stdin would eat input meant for the shell. Hand it EOF instead.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    // `kill_on_drop` reaches the process and nothing it started. A probe that is
    // a wrapper script has children of its own, and killing the script leaves
    // them running -- so put the probe in a process group of its own, which
    // makes the whole tree signallable by one call on the timeout path below.
    #[cfg(unix)]
    command.process_group(0);

    let Ok(mut child) = command.spawn() else {
        return ProbeOutput::Failed;
    };

    let group = child.id();
    let stdout = child.stdout.take().expect("stdout was piped");

    let probe = async move {
        use tokio::io::AsyncReadExt as _;

        let mut output = Vec::new();
        stdout
            .take(MAX_PROBE_OUTPUT)
            .read_to_end(&mut output)
            .await
            .ok()?;

        // Filling the cap means the executable is still talking, so waiting for
        // it to exit could block until the timeout on a child that is only
        // blocked writing into a pipe nobody is draining any more. It has
        // already failed to answer like a Stellar CLI; say so now.
        if output.len() as u64 >= MAX_PROBE_OUTPUT {
            return None;
        }

        let status = child.wait().await.ok()?;

        status
            .success()
            .then(|| String::from_utf8_lossy(&output).into_owned())
    };

    match tokio::time::timeout(INSTALL_PROBE_TIMEOUT, probe).await {
        Ok(Some(output)) => ProbeOutput::Answered(output),
        // The child is dropped with the future, so `kill_on_drop` has stopped
        // it; this reaches anything it started, which matters for the capped
        // case above just as much as for a timeout.
        Ok(None) => {
            kill_process_group(group);
            ProbeOutput::Failed
        }
        Err(_) => {
            // The future has been dropped by now, so `kill_on_drop` has already
            // signalled the process itself; this reaches whatever it started.
            kill_process_group(group);
            ProbeOutput::TimedOut
        }
    }
}

/// Kill the process group `pid` leads, which `process_group(0)` above made it
/// the leader of.
#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return;
    };

    // Negating the pid addresses the group. Failure is expected and ignorable:
    // the group is gone as soon as its last member is, which the kill on drop
    // may already have accomplished.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// Nothing to do beyond the kill on drop: without process groups there is no
/// way to reach a probe's children, and no `process_group` call was made above.
#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}

fn parse_only_version(output: &str) -> Option<String> {
    let version = output.trim();

    // An older CLI treats `--only-version` as an unknown argument and may still
    // exit zero while printing usage, so require a bare version.
    Version::parse(version).ok().map(|_| version.to_string())
}

/// Pull the version out of a banner like `stellar 22.8.0 (18f54cd...)`.
fn parse_version_banner(output: &str) -> Option<String> {
    output
        .lines()
        .next()?
        .split_whitespace()
        .find(|token| Version::parse(token).is_ok())
        .map(ToString::to_string)
}

fn check_rust_version(print: &Print) {
    match version() {
        Ok(rust_version) => {
            let v184 = Version::parse("1.84.0").unwrap();
            let v182 = Version::parse("1.82.0").unwrap();

            if rust_version >= v182 && rust_version < v184 {
                print.errorln(format!(
                    "Rust {rust_version} cannot be used to build contracts"
                ));
            } else {
                print.infoln(format!("Rust version: {rust_version}"));
            }
        }
        Err(_) => {
            print.warnln("Could not determine Rust version".to_string());
        }
    }
}

fn check_wasm_target(print: &Print) {
    let expected_target = get_expected_wasm_target();

    let Ok(output) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    else {
        print.warnln("Could not retrieve Rust targets".to_string());
        return;
    };

    if output.status.success() {
        let targets = String::from_utf8_lossy(&output.stdout);

        if targets.lines().any(|line| line.trim() == expected_target) {
            print.checkln(format!("Rust target `{expected_target}` is installed"));
        } else {
            print.errorln(format!("Rust target `{expected_target}` is not installed"));
        }
    } else {
        print.warnln("Could not retrieve Rust targets".to_string());
    }
}

fn check_container_engine(print: &Print) {
    // `resolved_default` silently falls back to docker on a bad
    // `STELLAR_CONTAINER_ENGINE`, so probe the raw value first to warn about a
    // typo instead of reporting a phantom docker as available.
    if !Engine::is_valid_engine() {
        let value = std::env::var("STELLAR_CONTAINER_ENGINE").unwrap_or_default();
        print.warnln(format!(
            "Unknown container engine `{value}`; expected one of: {}",
            Engine::supported_engines()
        ));
        return;
    }

    let engine = Engine::resolved_default();

    // `output()` succeeds as long as the binary spawned, regardless of exit
    // status, so it tells us whether the engine is installed on `PATH`.
    if Command::new(engine.program())
        .arg("--version")
        .output()
        .is_ok()
    {
        print.checkln(format!("Container engine `{engine}` is available"));
    } else {
        print.warnln(format!("Container engine `{engine}` is not installed"));
    }
}

fn check_optional_features(print: &Print) {
    #[cfg(feature = "additional-libs")]
    {
        print.checkln("Wasm optimization");
        print.checkln("Secure store (OS keyring)");
        print.checkln("Ledger hardware wallet");
    }

    #[cfg(not(feature = "additional-libs"))]
    {
        print.warnln(
            "The following features are disabled until `--features additional-libs` is used:",
        );
        print.blankln("- Wasm optimization");
        print.blankln("- Secure store (OS keyring)");
        print.blankln("- Ledger hardware wallet");
    }
}

fn get_expected_wasm_target() -> String {
    let Ok(current_version) = version() else {
        return "wasm32v1-none".into();
    };

    let v184 = Version::parse("1.84.0").unwrap();

    if current_version < v184 {
        "wasm32-unknown-unknown".into()
    } else {
        "wasm32v1-none".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_only_version_output() {
        assert_eq!(parse_only_version("27.1.0\n").as_deref(), Some("27.1.0"));
    }

    #[test]
    fn rejects_usage_text_printed_for_an_unknown_flag() {
        // A CLI old enough to reject `--only-version` must not be reported as
        // if the usage text were its version.
        let usage = "error: unexpected argument '--only-version' found\n\n\
                     Usage: soroban version [OPTIONS]\n";

        assert_eq!(parse_only_version(usage), None);
    }

    #[test]
    fn parses_version_from_an_older_cli_banner() {
        // Real output from a 22.8.0 `soroban`, which predates `--only-version`.
        let banner = "stellar 22.8.0 (18f54cdc0342726eb13ac14f84e899703d9f180a)\n\
                      stellar-xdr 22.1.0 (e139229708)\n\
                      xdr curr (529d5176f2)\n";

        assert_eq!(parse_version_banner(banner).as_deref(), Some("22.8.0"));
    }

    #[test]
    fn returns_none_for_a_banner_without_a_version() {
        assert_eq!(parse_version_banner("not a stellar cli\n"), None);
    }

    /// `None` stands for a probe that failed rather than one that timed out.
    /// `summarize_versions` treats the two alike -- both are executables it
    /// never heard from -- so the cases it does distinguish are all reachable
    /// through this shorter spelling. `timed_out_counts_as_unanswered` covers
    /// the other outcome directly.
    fn installs(entries: &[(&str, Option<&str>)]) -> Vec<Install> {
        entries
            .iter()
            .map(|(path, version)| {
                let version = version.map_or(InstallVersion::Silent, |version| {
                    InstallVersion::Known(version.to_string())
                });

                (PathBuf::from(path), version)
            })
            .collect()
    }

    #[test]
    fn timed_out_counts_as_unanswered_not_as_a_disagreement() {
        // A hung executable told us nothing about its version, so it cannot be
        // one of the versions observed to differ -- the same standing as one
        // that failed to run, however differently it is reported to the user.
        assert_eq!(
            summarize_versions(&[
                (
                    PathBuf::from("/a/stellar"),
                    InstallVersion::Known("27.1.0".to_string())
                ),
                (PathBuf::from("/b/soroban"), InstallVersion::TimedOut),
            ]),
            InstalledVersions::Unanswered {
                agreed: Some("27.1.0".to_string()),
                unanswered: 1
            }
        );
    }

    fn known(version: &str) -> InstallVersion {
        InstallVersion::Known(version.to_string())
    }

    #[test]
    fn marks_an_install_behind_the_latest_release_as_outdated() {
        let latest = Version::parse("27.1.0").unwrap();

        assert_eq!(
            install_label(&known("22.8.0"), Some(&latest)),
            "22.8.0, outdated -- 27.1.0 is available"
        );
    }

    #[test]
    fn does_not_mark_an_install_at_or_past_the_latest_release() {
        let latest = Version::parse("27.1.0").unwrap();

        // At the latest: nothing to say beyond the version.
        assert_eq!(install_label(&known("27.1.0"), Some(&latest)), "27.1.0");
        // Ahead of it -- a local build, or a release newer than the cache knows.
        // Not outdated, and not something to invent a verdict about.
        assert_eq!(install_label(&known("28.0.0"), Some(&latest)), "28.0.0");
    }

    #[test]
    fn says_nothing_about_currency_when_the_latest_release_is_unknown() {
        // An offline run with no cache to fall back on. Every install here is
        // unjudged, not current -- claiming otherwise would be the reassuring
        // lie this whole report exists to avoid.
        assert_eq!(install_label(&known("22.8.0"), None), "22.8.0");
        assert_eq!(known("22.8.0").is_behind(None), None);
    }

    #[test]
    fn says_nothing_about_currency_for_an_executable_that_did_not_answer() {
        let latest = Version::parse("27.1.0").unwrap();

        // No version to compare, so no verdict -- and the reason it could not be
        // asked survives into the label unchanged.
        assert_eq!(InstallVersion::Silent.is_behind(Some(&latest)), None);
        assert_eq!(InstallVersion::TimedOut.is_behind(Some(&latest)), None);
        assert_eq!(
            install_label(&InstallVersion::TimedOut, Some(&latest)),
            "hung; killed after timing out"
        );
    }

    #[test]
    fn compares_prereleases_by_semver_precedence() {
        let latest = Version::parse("27.1.0").unwrap();

        // A release candidate for the latest version is still behind it, which
        // is what semver precedence says and what the user needs to hear.
        assert_eq!(known("27.1.0-rc.1").is_behind(Some(&latest)), Some(true));
    }

    #[test]
    fn a_probe_outcome_labels_itself_distinctly() {
        // The user has to be able to tell a hung executable from an unreadable
        // one in the listing; that distinction is the whole point of keeping
        // `TimedOut` apart from `Silent`.
        assert_eq!(
            InstallVersion::Known("27.1.0".to_string()).label(),
            "27.1.0"
        );
        assert_ne!(
            InstallVersion::TimedOut.label(),
            InstallVersion::Silent.label()
        );
    }

    #[test]
    fn agreement_needs_every_executable_to_have_answered() {
        assert_eq!(
            summarize_versions(&installs(&[
                ("/a/stellar", Some("27.1.0")),
                ("/a/soroban", Some("27.1.0")),
            ])),
            InstalledVersions::Agreed("27.1.0".to_string())
        );
    }

    #[test]
    fn distinct_known_versions_disagree() {
        assert_eq!(
            summarize_versions(&installs(&[
                ("/a/stellar", Some("27.1.0")),
                ("/b/soroban", Some("22.8.0")),
            ])),
            InstalledVersions::Disagree { unanswered: 0 }
        );
    }

    #[test]
    fn executables_that_cannot_be_asked_are_not_a_disagreement() {
        // Two unrunnable binaries tell us nothing about whether their versions
        // match, so reporting a disagreement would invent a cause.
        assert_eq!(
            summarize_versions(&installs(&[("/a/stellar", None), ("/b/soroban", None)])),
            InstalledVersions::Unanswered {
                agreed: None,
                unanswered: 2
            }
        );

        // One answered, the other did not: still nothing to contradict.
        assert_eq!(
            summarize_versions(&installs(&[
                ("/a/stellar", Some("27.1.0")),
                ("/b/soroban", None),
            ])),
            InstalledVersions::Unanswered {
                agreed: Some("27.1.0".to_string()),
                unanswered: 1
            }
        );
    }

    #[test]
    fn a_failed_probe_does_not_erase_the_agreement_around_it() {
        // Two agree and a third could not be asked. The agreement is the most
        // useful thing known about this machine, and it survives the probe that
        // failed beside it.
        assert_eq!(
            summarize_versions(&installs(&[
                ("/a/stellar", Some("27.1.0")),
                ("/a/soroban", Some("27.1.0")),
                ("/b/stellar", None),
            ])),
            InstalledVersions::Unanswered {
                agreed: Some("27.1.0".to_string()),
                unanswered: 1
            }
        );
    }

    #[test]
    fn an_observed_disagreement_outranks_a_failed_probe() {
        // The executable that never answered is counted apart from the two that
        // did: it is not one of the versions observed to differ.
        assert_eq!(
            summarize_versions(&installs(&[
                ("/a/stellar", Some("27.1.0")),
                ("/b/soroban", Some("22.8.0")),
                ("/c/stellar", None),
            ])),
            InstalledVersions::Disagree { unanswered: 1 }
        );
    }
}
