//! # Youki
//! Container Runtime written in Rust, inspired by [railcar](https://github.com/oracle/railcar)
//! This crate provides a container runtime which can be used by a high-level container runtime to run containers.

use std::fs;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Result;
use clap::{crate_version, Clap};

use nix::sys::stat::Mode;
use nix::unistd::getuid;
use youki::commands::create;
use youki::commands::delete;
use youki::commands::events;
use youki::commands::exec;
use youki::commands::info;
use youki::commands::kill;
use youki::commands::list;
use youki::commands::pause;
use youki::commands::ps;
use youki::commands::resume;
use youki::commands::run;
use youki::commands::spec_json;
use youki::commands::start;
use youki::commands::state;
use youki::rootless::rootless_required;
use youki::utils::{self, create_dir_all_with_mode};

// High-level commandline option definition
// This takes global options as well as individual commands as specified in [OCI runtime-spec](https://github.com/opencontainers/runtime-spec/blob/master/runtime.md)
// Also check [runc commandline documentation](https://github.com/opencontainers/runc/blob/master/man/runc.8.md) for more explanation
#[derive(Clap, Debug)]
#[clap(version = crate_version!(), author = "youki team")]
struct Opts {
    /// root directory to store container state
    #[clap(short, long)]
    root: Option<PathBuf>,
    #[clap(short, long)]
    log: Option<PathBuf>,
    #[clap(long)]
    log_format: Option<String>,
    /// Enable systemd cgroup manager, rather then use the cgroupfs directly.
    #[clap(short, long)]
    systemd_cgroup: bool,
    /// command to actually manage container
    #[clap(subcommand)]
    subcmd: SubCommand,
}

// Subcommands accepted by Youki, confirming with [OCI runtime-spec](https://github.com/opencontainers/runtime-spec/blob/master/runtime.md)
// Also for a short information, check [runc commandline documentation](https://github.com/opencontainers/runc/blob/master/man/runc.8.md)
#[derive(Clap, Debug)]
enum SubCommand {
    #[clap(version = crate_version!(), author = "youki team")]
    Create(create::Create),
    #[clap(version = crate_version!(), author = "youki team")]
    Start(start::Start),
    #[clap(version = crate_version!(), author = "youki team")]
    Run(run::Run),
    #[clap(version = crate_version!(), author = "youki team")]
    Exec(exec::Exec),
    #[clap(version = crate_version!(), author = "youki team")]
    Kill(kill::Kill),
    #[clap(version = crate_version!(), author = "youki team")]
    Delete(delete::Delete),
    #[clap(version = crate_version!(), author = "youki team")]
    State(state::State),
    #[clap(version = crate_version!(), author = "youki team")]
    Info(info::Info),
    #[clap(version = crate_version!(), author = "youki team")]
    Spec(spec_json::SpecJson),
    #[clap(version = crate_version!(), author = "youki team")]
    List(list::List),
    #[clap(version = crate_version!(), author = "youki team")]
    Pause(pause::Pause),
    #[clap(version = crate_version!(), author = "youki team")]
    Resume(resume::Resume),
    #[clap(version = crate_version!(), author = "youki team")]
    Events(events::Events),
    #[clap(version = crate_version!(), author = "youki team", setting=clap::AppSettings::AllowLeadingHyphen)]
    Ps(ps::Ps),
}

/// This is the entry point in the container runtime. The binary is run by a high-level container runtime,
/// with various flags passed. This parses the flags, creates and manages appropriate resources.
fn main() -> Result<()> {
    let opts = Opts::parse();

    if let Err(e) = youki::logger::init(opts.log) {
        eprintln!("log init failed: {:?}", e);
    }

    let root_path = determine_root_path(opts.root)?;
    let systemd_cgroup = opts.systemd_cgroup;

    match opts.subcmd {
        SubCommand::Create(create) => create.exec(root_path, systemd_cgroup),
        SubCommand::Start(start) => start.exec(root_path),
        SubCommand::Run(run) => run.exec(root_path, systemd_cgroup),
        SubCommand::Exec(exec) => exec.exec(root_path),
        SubCommand::Kill(kill) => kill.exec(root_path),
        SubCommand::Delete(delete) => delete.exec(root_path, systemd_cgroup),
        SubCommand::State(state) => state.exec(root_path),
        SubCommand::Info(info) => info.exec(),
        SubCommand::List(list) => list.exec(root_path),
        SubCommand::Spec(spec) => spec.exec(),
        SubCommand::Pause(pause) => pause.exec(root_path, systemd_cgroup),
        SubCommand::Resume(resume) => resume.exec(root_path, systemd_cgroup),
        SubCommand::Events(events) => events.exec(root_path),
        SubCommand::Ps(ps) => ps.exec(root_path),
    }
}

fn determine_root_path(root_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = root_path {
        return Ok(path);
    }

    if !rootless_required() {
        let default = PathBuf::from("/run/youki");
        utils::create_dir_all(&default)?;
        return Ok(default);
    }

    // see https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html
    if let Ok(path) = std::env::var("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(path));
    }

    // XDG_RUNTIME_DIR is not set, try the usual location
    let uid = getuid().as_raw();
    let runtime_dir = PathBuf::from(format!("/run/user/{}", uid));
    if create_dir_all_with_mode(&runtime_dir, uid, Mode::S_IRWXU).is_ok() {
        return Ok(runtime_dir);
    }

    if let Ok(path) = std::env::var("HOME") {
        let home = PathBuf::from(path);
        if let Ok(resolved) = fs::canonicalize(home) {
            let run_dir = resolved.join(".youki/run");
            if create_dir_all_with_mode(&run_dir, uid, Mode::S_IRWXU).is_ok() {
                return Ok(run_dir);
            }
        }
    }

    let tmp_dir = PathBuf::from(format!("/tmp/youki/{}", uid));
    if create_dir_all_with_mode(&tmp_dir, uid, Mode::S_IRWXU).is_ok() {
        return Ok(tmp_dir);
    }

    bail!("could not find a storage location with suitable permissions for the current user");
}
