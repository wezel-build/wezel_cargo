use std::io::{BufRead, BufReader, Write};
use std::process;

use anyhow::{Context, Result, anyhow};
use forager_sdk::{Forager, ForagerPluginOutput};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum PackageSpecifier {
    Packages(Vec<String>),
    Workspace(WorkspaceTag),
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WorkspaceTag {
    Workspace,
}

/// Selects which targets of a given kind to build — either the literal string
/// "all" or a list of target names.
#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum TargetSpecifier {
    Names(Vec<String>),
    All(AllTag),
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum AllTag {
    All,
}

impl TargetSpecifier {
    /// Appends the appropriate cargo flags to `child`. `all_flag` is the plural
    /// flag that selects every target of the kind (e.g. `--examples`), and
    /// `one_flag` is the singular flag that selects one by name (e.g. `--example`).
    fn append(&self, child: &mut process::Command, all_flag: &str, one_flag: &str) {
        match self {
            TargetSpecifier::All(AllTag::All) => {
                child.arg(all_flag);
            }
            TargetSpecifier::Names(names) => {
                for name in names {
                    child.arg(one_flag).arg(name);
                }
            }
        }
    }
}

#[derive(Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Command {
    #[default]
    Build,
    Bench,
    Test,
}

impl Command {
    fn as_str(&self) -> &'static str {
        match self {
            Command::Build => "build",
            Command::Bench => "bench",
            Command::Test => "test",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct CargoInputs {
    /// Cargo subcommand to invoke (`build`, `bench`, or `test`).
    command: Command,
    /// Build target — either a list of package names or the literal string "workspace".
    build_target: PackageSpecifier,
    /// Cargo profile to use, propagated as `--profile <value>`. Omit to use the default profile.
    #[serde(default)]
    profile: Option<String>,
    /// Build every target (lib, bins, tests, benches, examples) via `--all-targets`.
    #[serde(default)]
    all_targets: bool,
    /// Build the library target via `--lib`.
    #[serde(default)]
    lib: bool,
    /// Examples to build, as `--examples` (all) or `--example <name>` per name. Omit to build none.
    #[serde(default)]
    examples: Option<TargetSpecifier>,
    /// Benchmarks to build, as `--benches` (all) or `--bench <name>` per name. Omit to build none.
    #[serde(default)]
    benches: Option<TargetSpecifier>,
    /// Test targets to build, as `--tests` (all) or `--test <name>` per name. Omit to build none.
    #[serde(default)]
    tests: Option<TargetSpecifier>,
    /// Binaries to build, as `--bins` (all) or `--bin <name>` per name. Omit to build none.
    #[serde(default)]
    bins: Option<TargetSpecifier>,
}

/// Renders a spawned command back into a readable invocation string for error
/// messages, e.g. `cargo build --workspace --example foo`.
fn render_command(cmd: &process::Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses the duration out of cargo's `Finished` status line, returning
/// milliseconds. Cargo formats the duration (see its `util::elapsed`) as either
/// `"<secs>.<centis>s"` for sub-minute builds (e.g. `0.26s`) or
/// `"<mins>m <secs>s"` for longer ones (e.g. `1m 05s`, no sub-second part).
/// Returns `None` for any line that isn't such a status line.
fn parse_finished_ms(line: &str) -> Option<u64> {
    if !line.trim_start().starts_with("Finished") {
        return None;
    }
    // The rest of the line (profile, target kinds) never contains " in ", so
    // splitting from the right isolates the duration.
    let dur = line.rsplit_once(" in ")?.1.trim();
    if let Some((mins, secs)) = dur.split_once("m ") {
        let mins: u64 = mins.trim().parse().ok()?;
        let secs: u64 = secs.trim().strip_suffix('s')?.trim().parse().ok()?;
        Some(mins * 60_000 + secs * 1_000)
    } else {
        let secs: f64 = dur.strip_suffix('s')?.parse().ok()?;
        Some((secs * 1_000.0).round() as u64)
    }
}

struct Cargo;

impl Forager for Cargo {
    const NAME: &'static str = "cargo";
    const DESCRIPTION: &'static str =
        "Runs `cargo <command>` and records the build time cargo reports";
    const OUTCOMES_DOC: &'static str = "**`time_ms`** — cargo's own reported build duration (the `Finished … in <t>` \
         line), in milliseconds. Excludes process startup and dependency resolution.";
    type Inputs = CargoInputs;

    fn run(inputs: CargoInputs) -> Result<Vec<ForagerPluginOutput>> {
        let mut child = process::Command::new("cargo");
        child.arg(inputs.command.as_str());
        if let Some(profile) = inputs.profile.as_deref() {
            child.arg("--profile").arg(profile);
        }
        match inputs.build_target {
            PackageSpecifier::Packages(items) => {
                items
                    .into_iter()
                    .fold(&mut child, |child, package| child.arg("-p").arg(package));
            }
            PackageSpecifier::Workspace(WorkspaceTag::Workspace) => {
                child.arg("--workspace");
            }
        }

        if inputs.all_targets {
            child.arg("--all-targets");
        }
        if inputs.lib {
            child.arg("--lib");
        }
        if let Some(examples) = &inputs.examples {
            examples.append(&mut child, "--examples", "--example");
        }
        if let Some(benches) = &inputs.benches {
            benches.append(&mut child, "--benches", "--bench");
        }
        if let Some(tests) = &inputs.tests {
            tests.append(&mut child, "--tests", "--test");
        }
        if let Some(bins) = &inputs.bins {
            bins.append(&mut child, "--bins", "--bin");
        }

        // Capture cargo's stderr so we can read the duration out of its
        // `Finished ... in <t>` status line, which cargo measures itself and
        // which therefore excludes our process-spawn and cargo's own startup
        // overhead. We still forward every line back to stderr so the build
        // output the user sees is unchanged. stdout stays inherited (e.g. so
        // `cargo test` runner output is untouched).
        child.stderr(process::Stdio::piped());

        let timer_start = std::time::Instant::now();
        let mut proc = child.spawn().context("failed to spawn cargo")?;
        let stderr = proc.stderr.take().expect("stderr was piped");

        let mut reported_ms: Option<u64> = None;
        let sink = std::io::stderr();
        for line in BufReader::new(stderr).lines() {
            let line = line.context("failed to read cargo stderr")?;
            if reported_ms.is_none() {
                reported_ms = parse_finished_ms(&line);
            }
            let _ = writeln!(sink.lock(), "{line}");
        }

        let status = proc.wait().context("failed to wait for cargo")?;
        if !status.success() {
            return Err(anyhow!("`{}` exited with {status}", render_command(&child)));
        }

        // Fall back to wall-clock only if cargo printed no parseable `Finished`
        // line (it always does on success, but keep the metric non-null).
        let ms: u64 = match reported_ms {
            Some(ms) => ms,
            None => timer_start
                .elapsed()
                .as_millis()
                .try_into()
                .context("build duration overflowed u64")?,
        };
        Ok(vec![ForagerPluginOutput {
            name: "time_ms".to_owned(),
            value: ms.into(),
            tags: Default::default(),
        }])
    }
}

fn main() {
    if let Err(error) = forager_sdk::run::<Cargo>() {
        eprintln!("{}: {error:#}", env!("CARGO_BIN_NAME"));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> CargoInputs {
        serde_json::from_str(s).expect("should parse")
    }

    #[test]
    fn workspace_target_from_string() {
        let inputs = parse(r#"{"command":"build","build_target":"workspace"}"#);
        assert!(matches!(inputs.command, Command::Build));
        assert!(matches!(
            inputs.build_target,
            PackageSpecifier::Workspace(WorkspaceTag::Workspace)
        ));
    }

    #[test]
    fn packages_target_from_array() {
        let inputs = parse(r#"{"command":"test","build_target":["wezel_cargo","wezel_types"]}"#);
        assert!(matches!(inputs.command, Command::Test));
        match inputs.build_target {
            PackageSpecifier::Packages(items) => {
                assert_eq!(items, vec!["wezel_cargo", "wezel_types"]);
            }
            _ => panic!("expected Packages variant"),
        }
    }

    #[test]
    fn command_is_lowercase() {
        assert!(
            serde_json::from_str::<CargoInputs>(
                r#"{"command":"Build","build_target":"workspace"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn workspace_tag_is_lowercase() {
        assert!(
            serde_json::from_str::<CargoInputs>(
                r#"{"command":"build","build_target":"WORKSPACE"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn null_build_target_rejected() {
        assert!(
            serde_json::from_str::<CargoInputs>(r#"{"command":"build","build_target":null}"#)
                .is_err()
        );
    }

    #[test]
    fn missing_build_target_rejected() {
        assert!(serde_json::from_str::<CargoInputs>(r#"{"command":"build"}"#).is_err());
    }

    #[test]
    fn profile_defaults_to_none() {
        let inputs = parse(r#"{"command":"build","build_target":"workspace"}"#);
        assert!(inputs.profile.is_none());
    }

    #[test]
    fn profile_passed_through() {
        let inputs = parse(r#"{"command":"build","build_target":"workspace","profile":"release"}"#);
        assert_eq!(inputs.profile.as_deref(), Some("release"));
    }

    #[test]
    fn target_selectors_default_to_none() {
        let inputs = parse(r#"{"command":"build","build_target":"workspace"}"#);
        assert!(inputs.examples.is_none());
        assert!(inputs.benches.is_none());
        assert!(inputs.tests.is_none());
        assert!(inputs.bins.is_none());
    }

    #[test]
    fn target_all_from_string() {
        let inputs = parse(r#"{"command":"build","build_target":"workspace","examples":"all"}"#);
        assert!(matches!(
            inputs.examples,
            Some(TargetSpecifier::All(AllTag::All))
        ));
    }

    #[test]
    fn target_names_from_array() {
        let inputs = parse(
            r#"{"command":"test","build_target":"workspace","tests":["integration","smoke"]}"#,
        );
        match inputs.tests {
            Some(TargetSpecifier::Names(names)) => {
                assert_eq!(names, vec!["integration", "smoke"]);
            }
            _ => panic!("expected Names variant"),
        }
    }

    #[test]
    fn target_all_tag_is_lowercase() {
        assert!(
            serde_json::from_str::<CargoInputs>(
                r#"{"command":"build","build_target":"workspace","bins":"ALL"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn all_targets_and_lib_default_to_false() {
        let inputs = parse(r#"{"command":"build","build_target":"workspace"}"#);
        assert!(!inputs.all_targets);
        assert!(!inputs.lib);
    }

    #[test]
    fn all_targets_and_lib_parsed() {
        let inputs = parse(
            r#"{"command":"build","build_target":"workspace","all_targets":true,"lib":true}"#,
        );
        assert!(inputs.all_targets);
        assert!(inputs.lib);
    }

    #[test]
    fn parse_finished_sub_minute() {
        let line = "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.26s";
        assert_eq!(parse_finished_ms(line), Some(260));
    }

    #[test]
    fn parse_finished_whole_seconds() {
        let line = "    Finished `release` profile [optimized] target(s) in 12.00s";
        assert_eq!(parse_finished_ms(line), Some(12_000));
    }

    #[test]
    fn parse_finished_minutes() {
        let line = "    Finished `release` profile [optimized] target(s) in 1m 05s";
        assert_eq!(parse_finished_ms(line), Some(65_000));
    }

    #[test]
    fn parse_finished_ignores_other_lines() {
        assert_eq!(parse_finished_ms("   Compiling wezel_cargo v0.1.0"), None);
        assert_eq!(parse_finished_ms("warning: unused variable: `x`"), None);
        assert_eq!(parse_finished_ms(""), None);
    }

    #[test]
    fn render_command_joins_program_and_args() {
        let mut cmd = process::Command::new("cargo");
        cmd.arg("build")
            .arg("--workspace")
            .arg("--example")
            .arg("foo");
        assert_eq!(
            render_command(&cmd),
            "cargo build --workspace --example foo"
        );
    }
}
