// arch: arch_C87Z9.5, arch_PCSQE.1, arch_QFQTD.1, arch_QFQTD.3
// orgasmic:arch_WZFAX, arch_QFQTD, arch_C87Z9
//! orgasmic CLI binary.
//!
//! The clap surface mirrors the inventory in `arch_006` (init, doctor,
//! update, serve, status, restart, project, board, tasks, task, run,
//! worker, prompt, skills, manager, tx, recovery, auth, question,
//! optional, hub, glossary, graph, decision, architecture, adr, snapshot, grill,
//! architect, plan, reconcile). TASK-005 promotes `serve`, `status`,
//! `restart`, `tx`, and `auth` to real implementations that talk to the
//! local daemon. Later tasks promote other groups as their owners land.

mod architecture_drift;
mod artifact;
mod content_lifecycle;
mod daemon_client;
mod daemon_lifecycle;
mod daemon_runtime;
mod daemon_service;
mod doctor;
mod goal;
mod home;
mod install_state;
mod manager;
mod member;
mod node;
mod path_env;
#[cfg(test)]
mod test_support;
mod update;

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use orgasmic_core::projects;
use orgasmic_daemon::{Daemon, DaemonOptions};

use crate::artifact::{cmd_artifact, ArtifactCmd};
use crate::content_lifecycle::{HubInstall, LifecycleEntry};
use crate::daemon_client::DaemonClient;
use crate::doctor::Finding;
use crate::goal::{cmd_goal, GoalCmd};
use crate::home::Home;
use crate::manager::{DispatchArgs, DispatchCloseArgs, DispatchStatusArgs};
use crate::member::{cmd_member, MemberCmd};
use crate::node::{cmd_node, NodeCmd};

#[derive(Parser, Debug)]
#[command(
    name = "orgasmic",
    version,
    about = "orgasmic — agent coordination on plain files"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Scaffold `$ORGASMIC_HOME` (config, dirs, secrets, sessions).
    #[command(after_help = "\
Examples:
  orgasmic init
  orgasmic doctor
  orgasmic project init --path ~/myrepo --name myrepo")]
    Init,
    /// Diagnose this install (home layout, shipped files, daemon liveness).
    #[command(after_help = "\
Examples:
  orgasmic doctor

A [warn] line means the install is usable but needs attention — e.g. stale daemon
uptime (see orgasmic status) or missing runtime content (see orgasmic init hints).")]
    Doctor {
        /// Re-mint duplicate identity ids introduced by a merge branch.
        /// Value is `base..incoming` (first parent keeps its id). Example:
        /// `--fix-id-collisions=main..feature/id-fix`
        #[arg(long = "fix-id-collisions")]
        fix_id_collisions: Option<String>,
        /// Project root to repair (defaults to source checkout).
        #[arg(long)]
        project: Option<PathBuf>,
        /// Repair fixable findings: relink a dangling source binary symlink and
        /// wire `$ORGASMIC_HOME/bin` onto PATH (env file + shell startup).
        #[arg(long)]
        fix: bool,
        /// With --fix, write the env file but never edit shell startup files.
        #[arg(long = "no-modify-path")]
        no_modify_path: bool,
    },
    /// Put the `orgasmic` CLI on your shell PATH.
    #[command(after_help = "\
Examples:
  orgasmic path ensure                 # wire $ORGASMIC_HOME/bin onto PATH
  orgasmic path ensure --no-modify-path
  orgasmic path print                  # print the line to add yourself")]
    Path {
        #[command(subcommand)]
        cmd: PathCmd,
    },
    /// Update the installed runtime or contributor source checkout.
    Update {
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long)]
        no_build: bool,
        /// Switch the runtime release channel (e.g. stable, nightly) and install
        /// that channel's head. Without it, the currently pinned channel is used.
        #[arg(long)]
        channel: Option<String>,
    },
    /// Per-project commands.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Show the global board.
    Board,
    /// Start the daemon (HTTP + WS), foreground.
    #[command(after_help = "\
Examples:
  orgasmic serve --port 4848

Bearer token path (created when the daemon first starts): ~/.orgasmic/user/auth/token")]
    Serve {
        /// Address to bind. Defaults to config.yaml or 127.0.0.1.
        #[arg(long)]
        bind: Option<IpAddr>,
        /// Port to bind. Defaults to config.yaml or 4848. Use `0` to let
        /// the OS pick a free port (printed on stdout).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Show daemon status (boot_id, projects, parse errors, tx count).
    #[command(after_help = "\
Examples:
  orgasmic status

Emits a [warn] prefix when the running daemon predates a newer binary or recent
daemon-code commits (same check as orgasmic doctor).")]
    Status,
    /// Restart the local daemon process.
    Restart(DaemonRestartArgs),
    /// Manage the local daemon lifecycle without a desktop app.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Open the daemon-hosted web UI.
    #[command(after_help = "\
Examples:
  orgasmic ui
  orgasmic ui --print-url")]
    Ui {
        /// Print a one-time launch URL instead of opening the browser.
        #[arg(long)]
        print_url: bool,
    },
    /// Task listing/inspection.
    Tasks {
        #[command(subcommand)]
        cmd: TasksCmd,
    },
    /// Task heading operations.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Worker run management.
    Run {
        #[command(subcommand)]
        cmd: RunCmd,
    },
    /// Worker inspection.
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Prompt template inspection and dry-run.
    Prompt {
        #[command(subcommand)]
        cmd: PromptCmd,
    },
    /// Skill discovery and inspection.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Optional shipped content lifecycle.
    Optional {
        #[command(subcommand)]
        cmd: OptionalCmd,
    },
    /// Hub content lifecycle.
    Hub {
        #[command(subcommand)]
        cmd: HubCmd,
    },
    /// Manager state and dispatch surface.
    Manager {
        #[command(subcommand)]
        cmd: ManagerCmd,
    },
    /// Tx append/list through the daemon.
    Tx {
        #[command(subcommand)]
        cmd: TxCmd,
    },
    /// Goal lifecycle (set / clear / supersede) through the daemon.
    Goal {
        #[command(subcommand)]
        cmd: GoalCmd,
    },
    /// Recovery status.
    Recovery {
        #[command(subcommand)]
        cmd: Option<RecoveryCmd>,
    },
    /// Bearer-token auth management.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Question / answer surface.
    Question {
        #[command(subcommand)]
        cmd: QuestionCmd,
    },
    /// Node identity helpers (daemon-free).
    Id {
        #[command(subcommand)]
        cmd: IdCmd,
    },
    /// Glossary terms.
    Glossary {
        #[command(subcommand)]
        cmd: GlossaryCmd,
    },
    /// Decision graph.
    Decision {
        #[command(subcommand)]
        cmd: DecisionCmd,
    },
    /// Architecture graph.
    Architecture {
        #[command(subcommand)]
        cmd: ArchitectureCmd,
    },
    /// Graph edge queries.
    Graph {
        #[command(subcommand)]
        cmd: GraphCmd,
    },
    /// Org node body edits through the daemon (OCC + structural guard).
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },
    /// Artifact store: block vocabulary, submit, and feedback.
    Artifact {
        #[command(subcommand)]
        cmd: ArtifactCmd,
    },
    /// Host-local member management (add / revoke / list). No daemon required.
    Member {
        #[command(subcommand)]
        cmd: MemberCmd,
    },
    /// Manager grilling stage.
    Grill {
        #[arg(long, default_value = "orgasmic")]
        project: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        wait: bool,
    },
    /// Manager architecture stage.
    Architect {
        #[arg(long, default_value = "orgasmic")]
        project: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        wait: bool,
    },
    /// Manager planning stage.
    Plan {
        #[arg(long, default_value = "orgasmic")]
        project: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PathCmd {
    /// Wire `$ORGASMIC_HOME/bin` onto PATH (managed env file + shell startup).
    Ensure {
        /// Write the env file but never edit shell startup files.
        #[arg(long = "no-modify-path")]
        no_modify_path: bool,
    },
    /// Print the line to add to your shell profile by hand.
    Print,
}

#[derive(Subcommand, Debug)]
enum ProjectCmd {
    /// Scaffold `.orgasmic/` in a repo and register it on the global board.
    #[command(after_help = "\
Examples:
  orgasmic project init --path ~/myrepo --name myrepo \\
    --default-branch main")]
    Init {
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "main")]
        default_branch: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        no_register: bool,
    },
    /// Register an existing `.orgasmic/` project on the global board.
    Add {
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// List projects registered on the global board.
    List,
}

#[derive(Subcommand, Debug)]
enum TasksCmd {
    /// List tasks for a project.
    List {
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    /// File a new task heading through the daemon.
    Create {
        /// Task id; omitted → daemon mints `TASK-XXXXX`.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        title: String,
        /// Org tags on the title line; repeatable.
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Additional `KEY=VALUE` properties; repeatable.
        #[arg(long = "property", value_name = "KEY=VALUE")]
        properties: Vec<String>,
    },
    /// Show one task.
    Get {
        id: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Update one task heading through the daemon.
    Update {
        id: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        priority: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Additional `KEY=VALUE` properties; repeatable.
        #[arg(long = "property", value_name = "KEY=VALUE")]
        properties: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum IdCmd {
    /// Print one minted node id to stdout (no daemon required).
    Mint {
        #[arg(long, value_enum)]
        class: MintClassArg,
    },
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum MintClassArg {
    Task,
    Decision,
    Architecture,
    Term,
    Artifact,
}

impl From<MintClassArg> for orgasmic_core::NodeIdClass {
    fn from(value: MintClassArg) -> Self {
        match value {
            MintClassArg::Task => Self::Task,
            MintClassArg::Decision => Self::Decision,
            MintClassArg::Architecture => Self::Architecture,
            MintClassArg::Term => Self::Term,
            MintClassArg::Artifact => Self::Artifact,
        }
    }
}

#[derive(Subcommand, Debug)]
enum GlossaryCmd {
    /// List glossary terms for a project.
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one glossary term.
    Get {
        id: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Create a glossary term through the daemon.
    Create {
        /// Term id; omitted → daemon mints `term_XXXXX`.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        definition: Option<String>,
        #[arg(long)]
        canonical: Option<String>,
        #[arg(long)]
        avoid: Option<String>,
        #[arg(long = "relates-to")]
        relates_to: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Additional `KEY=VALUE` properties; repeatable.
        #[arg(long = "property", value_name = "KEY=VALUE")]
        properties: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum DecisionCmd {
    /// List decision nodes for a project.
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one decision node.
    Get {
        id: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Create a decision node through the daemon.
    Create {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
        #[arg(long = "property", value_name = "KEY=VALUE")]
        properties: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ArchitectureCmd {
    /// List architecture nodes for a project.
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one architecture node.
    Get {
        id: String,
        #[arg(long)]
        project: Option<String>,
    },
    /// Report architecture/source marker drift for the current repo.
    Drift {
        /// Emit machine-readable JSON for CI.
        #[arg(long)]
        json: bool,
    },
    /// Create an architecture node through the daemon.
    Create {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
        #[arg(long = "property", value_name = "KEY=VALUE")]
        properties: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum GraphCmd {
    /// List graph edges, optionally filtered by node, direction, kind, or relation alias.
    Edges {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        relation: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum TxCmd {
    /// Append a tx entry through the daemon.
    Record {
        /// Tx type (e.g. `manager.action`, `task.state_transitioned`).
        #[arg(long = "type", value_name = "TYPE")]
        ty: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        actor: Option<String>,
        #[arg(long)]
        machine: Option<String>,
        /// Stable idempotency key. If the same value is replayed, the
        /// daemon returns the original result without double-appending.
        #[arg(long = "request-id")]
        request_id: Option<String>,
        /// Additional `KEY=VALUE` properties; repeatable.
        #[arg(long = "extra", value_name = "KEY=VALUE")]
        extra: Vec<String>,
        /// Override the tx file path (defaults to `$ORGASMIC_HOME/state/tx/YYYY-MM.org`).
        #[arg(long = "tx-path")]
        tx_path: Option<PathBuf>,
    },
    /// List tx entries known to the daemon.
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// Print the path to the bearer token file.
    Show,
}

#[derive(Subcommand, Debug)]
enum WorkerCmd {
    /// List workers known to the daemon.
    List,
    /// Show one worker.
    Show { id: String },
}

#[derive(Subcommand, Debug)]
enum PromptCmd {
    /// List prompt specs.
    List,
    /// Show one prompt spec.
    Show { id: String },
    /// Compile a prompt spec.
    Compile {
        id: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        renderer: Option<String>,
        /// Additional slot value as KEY=VALUE; repeatable.
        #[arg(long = "value", value_name = "KEY=VALUE")]
        values: Vec<String>,
    },
    /// Lint a prompt spec.
    Lint {
        id: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        renderer: Option<String>,
        /// Additional slot value as KEY=VALUE; repeatable.
        #[arg(long = "value", value_name = "KEY=VALUE")]
        values: Vec<String>,
    },
    /// Fork a shipped prompt spec into the user override layer.
    Fork { id: String },
}

#[derive(Subcommand, Debug)]
enum SkillsCmd {
    /// List skills known to the daemon.
    List,
    /// Show one skill definition.
    Show { id: String },
}

#[derive(Subcommand, Debug)]
enum OptionalCmd {
    /// List optional shipped content packs.
    List,
    /// Enable an optional content pack.
    Enable { name: String },
    /// Disable an optional content pack.
    Disable { name: String },
}

#[derive(Subcommand, Debug)]
enum HubCmd {
    /// Install a hub content pack from a URL.
    Install {
        url: String,
        #[arg(long, default_value = "skills")]
        family: String,
    },
    /// List installed hub content packs.
    List,
    /// Remove a hub content pack.
    Remove { name: String },
}

#[derive(Subcommand, Debug)]
enum ManagerCmd {
    /// Show manager state for registered projects.
    State,
    /// Dispatch a worker for one or more tasks (worktree + tx + driver).
    Dispatch(DispatchArgs),
    /// Close an open dispatch (done or aborted).
    DispatchClose(DispatchCloseArgs),
    /// List open dispatches and run health.
    DispatchStatus(DispatchStatusArgs),
    /// Clear an orphaned dispatch lease (no live run). Never needs a daemon
    /// restart; refuses when a live run still holds the lease.
    LeaseRelease(manager::LeaseReleaseArgs),
}

#[derive(Subcommand, Debug)]
enum QuestionCmd {
    /// Ask a blocking question through the daemon.
    Ask {
        #[arg(long)]
        text: String,
    },
    /// Answer a pending question.
    Answer {
        id: String,
        #[arg(long)]
        text: String,
    },
}

#[derive(Subcommand, Debug)]
enum RunCmd {
    /// List worker runs.
    List,
    /// Show one worker run.
    Show { id: String },
    /// Recover an interrupted worker run with an explicit recovery action.
    ///
    /// `--action` is one of `reattach_tmux`, `resume_native_fork`, or
    /// `start_recovery_run`. Omit it to let the daemon execute the sole valid
    /// action or report the available choices.
    Recover {
        id: String,
        #[arg(long)]
        action: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long = "request-id")]
        request_id: Option<String>,
        #[arg(long)]
        force_inert: bool,
    },
}

#[derive(Subcommand, Debug)]
enum RecoveryCmd {
    /// Show recovery / rebuild status from the daemon.
    Status,
}

#[derive(Subcommand, Debug)]
enum DaemonCmd {
    /// Show local daemon process state without starting it.
    Status,
    /// Start the local daemon in the background if it is not running.
    Start,
    /// Stop the local daemon process.
    Stop {
        /// Stop even while a live manager run exists.
        #[arg(long)]
        force: bool,
    },
    /// Stop and start the local daemon process.
    Restart(DaemonRestartArgs),
}

#[derive(Args, Debug, Clone)]
struct DaemonRestartArgs {
    /// Retained for older scripts; controlled restarts are recovery-aware.
    #[arg(long)]
    force: bool,
    /// Build a local orgasmic checkout and run the daemon from that binary
    /// without changing the installed bundle/source mode.
    #[arg(
        long = "from-source",
        value_name = "CHECKOUT",
        conflicts_with = "clear_runtime_override"
    )]
    from_source: Option<PathBuf>,
    /// With --from-source, reuse the newest existing release binary instead of
    /// running cargo build --release first.
    #[arg(long = "no-build", requires = "from_source")]
    no_build: bool,
    /// Remove a temporary daemon runtime override before restarting.
    #[arg(long = "clear-runtime-override")]
    clear_runtime_override: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = Home::from_env()?;
    let log_default = match &cli.cmd {
        Cmd::Serve { .. } => orgasmic_daemon::DaemonConfig::load(&home)
            .map(|cfg| cfg.log_level)
            .unwrap_or_else(|_| "info".to_string()),
        _ => "info".to_string(),
    };
    orgasmic_daemon::init_tracing(&log_default);
    match cli.cmd {
        Cmd::Init => cmd_init(&home),
        Cmd::Doctor {
            fix_id_collisions,
            project,
            fix,
            no_modify_path,
        } => cmd_doctor(
            &home,
            fix_id_collisions.as_deref(),
            project.as_deref(),
            fix,
            no_modify_path,
        ),
        Cmd::Path { cmd } => cmd_path(&home, cmd),
        Cmd::Update {
            branch,
            no_build,
            channel,
        } => update::run(&home, &branch, !no_build, channel),
        Cmd::Project { cmd } => match cmd {
            ProjectCmd::Init {
                path,
                name,
                default_branch,
                force,
                no_register,
            } => cmd_project_init(&home, path, name, default_branch, force, no_register),
            ProjectCmd::Add { path } => cmd_project_add(&home, path),
            ProjectCmd::List => cmd_project_list(&home),
        },
        Cmd::Board => cmd_project_list(&home),
        Cmd::Serve { bind, port } => cmd_serve(&home, bind, port),
        Cmd::Status => cmd_status(&home),
        Cmd::Restart(args) => cmd_daemon_restart(&home, args),
        Cmd::Daemon { cmd } => cmd_daemon(&home, cmd),
        Cmd::Ui { print_url } => cmd_ui(&home, print_url),
        Cmd::Tasks { cmd } => cmd_tasks(&home, cmd),
        Cmd::Task { cmd } => cmd_task(&home, cmd),
        Cmd::Run { cmd } => cmd_run(&home, cmd),
        Cmd::Worker { cmd } => cmd_worker(&home, cmd),
        Cmd::Prompt { cmd } => cmd_prompt(&home, cmd),
        Cmd::Skills { cmd } => cmd_skills(&home, cmd),
        Cmd::Optional { cmd } => cmd_optional(&home, cmd),
        Cmd::Hub { cmd } => cmd_hub(&home, cmd),
        Cmd::Manager { cmd } => cmd_manager(&home, cmd),
        Cmd::Tx { cmd } => cmd_tx(&home, cmd),
        Cmd::Goal { cmd } => cmd_goal(&home, cmd),
        Cmd::Recovery { cmd } => match cmd.unwrap_or(RecoveryCmd::Status) {
            RecoveryCmd::Status => cmd_recovery(&home),
        },
        Cmd::Auth { cmd } => match cmd {
            AuthCmd::Show => cmd_auth_show(&home),
        },
        Cmd::Question { cmd } => cmd_question(&home, cmd),
        Cmd::Id { cmd } => cmd_id(cmd),
        Cmd::Glossary { cmd } => cmd_glossary(&home, cmd),
        Cmd::Decision { cmd } => cmd_decision(&home, cmd),
        Cmd::Architecture { cmd } => cmd_architecture(&home, cmd),
        Cmd::Graph { cmd } => cmd_graph(&home, cmd),
        Cmd::Node { cmd } => cmd_node(&home, cmd),
        Cmd::Artifact { cmd } => cmd_artifact(&home, cmd),
        Cmd::Member { cmd } => cmd_member(&home, cmd),
        Cmd::Grill {
            project,
            reason,
            wait,
        } => cmd_stage(&home, "grill", project, reason, wait),
        Cmd::Architect {
            project,
            reason,
            wait,
        } => cmd_stage(&home, "architect", project, reason, wait),
        Cmd::Plan {
            project,
            reason,
            wait,
        } => cmd_stage(&home, "plan", project, reason, wait),
    }
}

fn cmd_init(home: &Home) -> Result<()> {
    home.ensure()?;
    println!("✓ orgasmic home ready at {}", home.root.display());
    println!("  config:   {}", home.config().display());
    println!("  runtime:  {}", home.current_runtime().display());
    println!(
        "  content:  {} (compatibility link)",
        home.source().display()
    );
    println!("  user:     {} (overrides go here)", home.user().display());
    println!("  state:    {}", home.state().display());
    println!("  sessions: per-project under <project>/.orgasmic/tmp/sessions/");
    println!("  secrets:  {} (gitignored)", home.secrets().display());
    println!("  logs:     {}", home.logs().display());
    println!("  bin:      {}", home.bin().display());
    println!(
        "  token:    {} (generated when the daemon first starts)",
        home.auth_token().display()
    );

    if !home.source().is_dir() {
        println!(
            "hint: runtime content missing — run scripts/install.sh, or scripts/install.sh --from-source <checkout> for contributor mode, to create {}",
            home.source().display()
        );
    }
    if broken_bin_symlink(home) {
        println!("hint: broken binary symlink — run orgasmic update");
    }
    match ensure_source_project_registered(home)? {
        SourceProjectRegistration::Registered { id, path } => {
            println!("✓ registered default project {id} → {}", path.display());
        }
        SourceProjectRegistration::AlreadyRegistered { id } => {
            println!("✓ default project {id} already registered");
        }
        SourceProjectRegistration::NoSourceProject => {}
    }

    println!();
    println!("Next:");
    println!("  orgasmic doctor                                      # verify install");
    println!("  orgasmic status                                      # start/check daemon");
    println!("  orgasmic ui                                          # open daemon UI");
    println!("  cd path/to/your/repo && orgasmic project init        # adopt another project");
    Ok(())
}

enum SourceProjectRegistration {
    Registered { id: String, path: PathBuf },
    AlreadyRegistered { id: String },
    NoSourceProject,
}

fn ensure_source_project_registered(home: &Home) -> Result<SourceProjectRegistration> {
    let source = home.source();
    let project_org = source.join(".orgasmic/project.org");
    if !project_org.exists() {
        return Ok(SourceProjectRegistration::NoSourceProject);
    }
    let project = read_project_config(&project_org)?;
    if projects::read_board(home)?
        .iter()
        .any(|entry| entry.id == project.id)
    {
        return Ok(SourceProjectRegistration::AlreadyRegistered { id: project.id });
    }
    let branch = git_output(&source, &["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|value| value != "HEAD" && !value.is_empty())
        .or(project.default_branch)
        .unwrap_or_else(|| "main".to_string());
    projects::register_project(home, &source, &project.id, &branch)?;
    Ok(SourceProjectRegistration::Registered {
        id: project.id,
        path: source,
    })
}

struct ProjectConfig {
    id: String,
    default_branch: Option<String>,
}

fn read_project_config(path: &Path) -> Result<ProjectConfig> {
    let src = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let file = orgasmic_core::OrgFile::parse(src, path.to_string_lossy())?;
    let project = orgasmic_core::ProjectFile::from_org(&file, path.to_string_lossy().as_ref())?;
    Ok(ProjectConfig {
        id: project.id.to_string(),
        default_branch: read_config_default_branch(&path.with_file_name("config.org")),
    })
}

/// Read `:DEFAULT_BRANCH:` from a sibling `config.org`; `None` if the file is
/// absent, unparseable, or carries no non-empty branch (dec_051).
fn read_config_default_branch(config_org: &Path) -> Option<String> {
    let src = std::fs::read_to_string(config_org).ok()?;
    let file = orgasmic_core::OrgFile::parse(src, config_org.to_string_lossy()).ok()?;
    let config =
        orgasmic_core::ProjectConfig::from_org(&file, config_org.to_string_lossy().as_ref())
            .ok()?;
    config
        .default_branch
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn git_output(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn broken_bin_symlink(home: &Home) -> bool {
    let bin = home.bin_orgasmic();
    let Ok(meta) = std::fs::symlink_metadata(&bin) else {
        return false;
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    match std::fs::read_link(&bin) {
        Ok(target) => !(target.exists() || home.bin().join(&target).exists()),
        Err(_) => true,
    }
}

fn cmd_doctor(
    home: &Home,
    fix_id_collisions: Option<&str>,
    project: Option<&Path>,
    fix: bool,
    no_modify_path: bool,
) -> Result<()> {
    if let Some(spec) = fix_id_collisions {
        return cmd_doctor_fix_id_collisions(home, spec, project);
    }
    if fix {
        cmd_doctor_fix(home, no_modify_path)?;
    }
    let findings = doctor::diagnose(home);
    let mut fails = 0;
    for f in &findings {
        match f {
            Finding::Ok(s) => println!("[ok]   {s}"),
            Finding::Warn(s) => println!("[warn] {s}"),
            Finding::Fail(s) => {
                println!("[fail] {s}");
                fails += 1;
            }
        }
    }
    if fails > 0 {
        anyhow::bail!("doctor: {fails} failure(s)");
    }
    Ok(())
}

fn cmd_doctor_fix_id_collisions(home: &Home, spec: &str, project: Option<&Path>) -> Result<()> {
    let project_root = project
        .map(PathBuf::from)
        .or_else(|| {
            let source = home.source();
            if source.is_dir() {
                Some(source)
            } else {
                None
            }
        })
        .context("project root required (--project or installed source checkout)")?;
    let (base_ref, incoming_ref) = spec
        .split_once("..")
        .with_context(|| format!("--fix-id-collisions expects base..incoming, got `{spec}`"))?;
    let mappings = orgasmic_core::repair_id_collisions(&project_root, base_ref, incoming_ref)
        .map_err(|e| {
            if let orgasmic_core::IdRepairError::AmbiguousAttribution { id, detail } = &e {
                anyhow::anyhow!(
                    "ambiguous reference attribution for `{id}`: {detail}\n\
                     Resolve manually or narrow the git range so exactly one duplicate occurrence \
                     lies on the incoming side."
                )
            } else {
                anyhow::anyhow!("{e}")
            }
        })?;
    if mappings.is_empty() {
        println!("no duplicate identity ids found; repair is a no-op");
    } else {
        for mapping in &mappings {
            println!("re-minted {} -> {}", mapping.old_id, mapping.new_id);
        }
    }
    Ok(())
}

/// Repair fixable doctor findings: relink a dangling source binary symlink,
/// wire the CLI onto PATH, and restart a local daemon with stale auth.
fn cmd_doctor_fix(home: &Home, no_modify_path: bool) -> Result<()> {
    if let Some(source) = source_checkout_for_repair(home) {
        match path_env::relink_source_binary(home, &source) {
            Ok(bin) => println!(
                "→ relinked {} -> {}",
                home.bin_orgasmic().display(),
                bin.display()
            ),
            Err(e) => eprintln!("warning: could not relink source binary: {e}"),
        }
    }
    let report = path_env::ensure(home, no_modify_path)?;
    print_path_report(home, &report);
    if !daemon_lifecycle::local_lifecycle_externally_owned() {
        match daemon_lifecycle::repair_unauthorized_local_daemon(home)? {
            daemon_lifecycle::AuthRepairOutcome::Repaired(outcome) => {
                println!("→ repaired daemon auth by restarting the local daemon");
                print_start_outcome(home, &outcome);
            }
            daemon_lifecycle::AuthRepairOutcome::NotNeeded => {}
        }
    }
    Ok(())
}

/// A source checkout to relink against, or `None` for bundle installs (whose
/// `bin/orgasmic` points at `../current/bin/orgasmic` and is repaired by update).
fn source_checkout_for_repair(home: &Home) -> Option<PathBuf> {
    let candidate = install_state::read(home)
        .ok()
        .flatten()
        .and_then(|state| state.source_checkout)
        .unwrap_or_else(|| home.source());
    if candidate.join(".git").exists() || candidate.join("Cargo.toml").exists() {
        Some(candidate)
    } else {
        None
    }
}

fn cmd_path(home: &Home, cmd: PathCmd) -> Result<()> {
    match cmd {
        PathCmd::Ensure { no_modify_path } => {
            let report = path_env::ensure(home, no_modify_path)?;
            print_path_report(home, &report);
            Ok(())
        }
        PathCmd::Print => {
            println!("# add this to your shell profile (e.g. ~/.zprofile or ~/.bashrc):");
            println!("{}", path_env::source_line(home));
            Ok(())
        }
    }
}

fn print_path_report(home: &Home, report: &path_env::EnsureReport) {
    if report.env_file_written {
        println!("→ wrote {}", home.env_file().display());
    }
    for f in &report.rc_files_modified {
        println!("→ added PATH line to {}", f.display());
    }
    if let Some(link) = &report.shim_linked {
        println!(
            "→ linked {} -> {}",
            link.display(),
            home.bin_orgasmic().display()
        );
    }
    if let Some(link) = &report.shim_blocked {
        println!(
            "  note: {} exists and isn't managed by orgasmic; left as-is",
            link.display()
        );
    }
    if report.modify_path_skipped {
        println!("→ left shell startup files untouched (--no-modify-path)");
        println!("  add this line yourself: {}", path_env::source_line(home));
    } else if report.rc_files_modified.is_empty()
        && !report.env_file_written
        && report.shim_linked.is_none()
    {
        println!("→ already wired: {}", home.env_file().display());
    }
    if report.shim_linked.is_some() || report.shim_already {
        println!("  `orgasmic` now resolves in this shell — no new terminal needed");
    } else if !report.already_on_path && !report.modify_path_skipped {
        println!(
            "  open a new terminal or run `. {}` to use orgasmic in this shell",
            home.env_file().display()
        );
    }
}

fn cmd_project_init(
    home: &Home,
    path: Option<PathBuf>,
    name: Option<String>,
    default_branch: String,
    force: bool,
    no_register: bool,
) -> Result<()> {
    let project_root = match path {
        Some(p) => p,
        None => std::env::current_dir().context("cwd")?,
    };
    let mut inputs = projects::ScaffoldInputs::derive(&project_root, name);
    inputs.default_branch = default_branch.clone();
    let written = projects::init_project(home, &project_root, &inputs, force)?;
    println!(
        "✓ scaffolded {} files under {}/.orgasmic",
        written.len(),
        project_root.display()
    );
    for p in &written {
        println!("  + {}", p.display());
    }
    if !no_register {
        match projects::register_project(home, &project_root, &inputs.project_id, &default_branch) {
            Ok(()) => println!("✓ registered {} on the global board", inputs.project_id),
            Err(e) => println!("[warn] board register skipped: {e}"),
        }
    }
    Ok(())
}

fn cmd_project_add(home: &Home, path: Option<PathBuf>) -> Result<()> {
    let project_root = match path {
        Some(p) => p,
        None => std::env::current_dir().context("cwd")?,
    };
    let dotorg = project_root.join(".orgasmic/project.org");
    let config = read_project_config(&dotorg).with_context(|| {
        format!(
            "read {}: project does not appear to be initialized",
            dotorg.display()
        )
    })?;
    let branch = git_output(&project_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|value| value != "HEAD" && !value.is_empty())
        .or(config.default_branch)
        .unwrap_or_else(|| "main".to_string());
    projects::register_project(home, &project_root, &config.id, &branch)?;
    println!("✓ registered {} → {}", config.id, project_root.display());
    Ok(())
}

fn cmd_project_list(home: &Home) -> Result<()> {
    let entries = projects::read_board(home)?;
    if entries.is_empty() {
        println!("(no projects registered — run `orgasmic project init`)");
        return Ok(());
    }
    println!("{:<24} {:<8} {:<40} REPO_URL", "ID", "BRANCH", "PATH");
    for e in entries {
        let repo_url = git_output(&e.path, &["config", "--get", "remote.origin.url"])
            .filter(|value| !value.is_empty())
            .unwrap_or_default();
        println!(
            "{:<24} {:<8} {:<40} {}",
            e.id,
            e.branch,
            e.path.display(),
            repo_url
        );
    }
    Ok(())
}

fn cmd_serve(home: &Home, bind: Option<IpAddr>, port: Option<u16>) -> Result<()> {
    let opts = DaemonOptions {
        bind_override: bind,
        port_override: port,
        ..DaemonOptions::default()
    };
    let home_clone = home.clone();
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let running = Daemon::run(home_clone, opts).await?;
        println!(
            "✓ orgasmic daemon listening on http://{} (boot_id={})",
            running.addr, running.boot_id
        );
        println!("  token file: {}", home.auth_token().display());
        println!("  press Ctrl+C to stop");
        tokio::signal::ctrl_c().await.ok();
        let _ = running.shutdown.send(());
        let _ = running.join.await;
        Ok::<(), anyhow::Error>(())
    })
}

fn cmd_status(home: &Home) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = client.get("/daemon/status").await?;
        let staleness = serde_json::from_value::<doctor::DaemonStatus>(value.clone())
            .ok()
            .and_then(|status| doctor::check_daemon_for_status_with_status(home, &status));
        println!("{}", serde_json::to_string_pretty(&value)?);
        if let Some(message) = staleness {
            eprintln!("[warn] {message}");
        }
        Ok::<(), anyhow::Error>(())
    })
}

fn cmd_daemon(home: &Home, cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Status => cmd_daemon_status(home),
        DaemonCmd::Start => {
            match daemon_lifecycle::start(home)? {
                outcome @ daemon_lifecycle::DaemonStartOutcome::Running(_) => {
                    println!("✓ daemon running");
                    print_start_outcome(home, &outcome);
                }
                outcome @ daemon_lifecycle::DaemonStartOutcome::StillBooting(_) => {
                    print_start_outcome(home, &outcome);
                }
            }
            Ok(())
        }
        DaemonCmd::Stop { force } => {
            match daemon_lifecycle::stop_with_force(home, force)? {
                Some(status) => {
                    println!("✓ daemon stopped");
                    println!("  pid:     {}", status.pid);
                    println!("  boot_id: {}", status.boot_id);
                }
                None => println!("daemon already stopped"),
            }
            Ok(())
        }
        DaemonCmd::Restart(args) => cmd_daemon_restart(home, args),
    }
}

fn print_daemon_persistence(home: &Home) {
    let persistence = daemon_lifecycle::persistence_status(home);
    println!("  adapter: {}", persistence.adapter);
    println!(
        "  persistence: installed={} enabled={}",
        yes_no(persistence.installed),
        yes_no(persistence.enabled)
    );
    if let Some(detail) = persistence.detail {
        println!("  persistence_detail: {detail}");
    }
    match daemon_runtime::read(home) {
        Ok(Some(runtime)) => println!("  runtime_override: {}", runtime.description()),
        Ok(None) => {}
        Err(error) => println!("  runtime_override: invalid ({error})"),
    }
}

fn print_start_outcome(home: &Home, outcome: &daemon_lifecycle::DaemonStartOutcome) {
    print_daemon_persistence(home);
    match outcome {
        daemon_lifecycle::DaemonStartOutcome::Running(status) => {
            println!("  pid:     {}", status.pid);
            println!("  boot_id: {}", status.boot_id);
        }
        daemon_lifecycle::DaemonStartOutcome::StillBooting(starting) => {
            println!("daemon still booting — check `orgasmic daemon status` shortly");
            println!("  pid:     {}", starting.pid);
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn cmd_daemon_status(home: &Home) -> Result<()> {
    match daemon_lifecycle::status(home)? {
        daemon_lifecycle::LocalDaemonState::Running(status) => {
            println!("running");
            print_daemon_persistence(home);
            println!("  pid:     {}", status.pid);
            println!("  boot_id: {}", status.boot_id);
        }
        daemon_lifecycle::LocalDaemonState::Starting(starting) => {
            println!("starting");
            print_daemon_persistence(home);
            println!("  pid:     {}", starting.pid);
        }
        daemon_lifecycle::LocalDaemonState::Down => {
            println!("stopped");
            print_daemon_persistence(home);
        }
        daemon_lifecycle::LocalDaemonState::Unauthorized => {
            println!("unauthorized");
            print_daemon_persistence(home);
        }
    }
    Ok(())
}

fn cmd_daemon_restart(home: &Home, args: DaemonRestartArgs) -> Result<()> {
    prepare_daemon_restart_runtime(home, &args)?;
    match daemon_lifecycle::restart_with_force(home, args.force)? {
        outcome @ daemon_lifecycle::DaemonStartOutcome::Running(_) => {
            println!("✓ daemon restarted");
            print_start_outcome(home, &outcome);
        }
        outcome @ daemon_lifecycle::DaemonStartOutcome::StillBooting(_) => {
            print_start_outcome(home, &outcome);
        }
    }
    Ok(())
}

fn prepare_daemon_restart_runtime(home: &Home, args: &DaemonRestartArgs) -> Result<()> {
    if args.clear_runtime_override {
        if daemon_runtime::clear(home)? {
            println!("→ cleared daemon runtime override");
        } else {
            println!("→ no daemon runtime override was set");
        }
    }
    if let Some(checkout) = &args.from_source {
        let runtime = daemon_runtime::set_local_source(home, checkout, !args.no_build)?;
        println!("→ daemon runtime override set");
        println!("  {}", runtime.description());
    }
    Ok(())
}

fn cmd_ui(home: &Home, print_url: bool) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct UiSessionResponse {
        path: String,
    }

    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let url = runtime.block_on(async {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let response: UiSessionResponse = client
            .post_json("/auth/ui-session", &serde_json::json!({}))
            .await?;
        Ok::<_, anyhow::Error>(client.absolute_url(&response.path))
    })?;
    if print_url {
        println!("{url}");
        return Ok(());
    }
    open_url(&url)?;
    println!("✓ opened {url}");
    Ok(())
}

#[allow(clippy::needless_return)]
fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .with_context(|| format!("open {url}"))?;
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .with_context(|| format!("open {url}"))?;
        return Ok(());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .with_context(|| format!("open {url} with xdg-open"))?;
        return Ok(());
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        anyhow::bail!("no platform browser opener available; open {url}");
    }
}

fn cmd_tasks(home: &Home, cmd: TasksCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = match cmd {
            TasksCmd::List { project } => {
                let project = manager::resolve_project(project)?;
                client.get(&format!("/projects/{project}/tasks")).await?
            }
        };
        print_json(&value)
    })
}

fn cmd_task(home: &Home, cmd: TaskCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = match cmd {
            TaskCmd::Create {
                id,
                project,
                title,
                tag,
                body,
                reason,
                request_id,
                properties,
            } => {
                let project = manager::resolve_project(project)?;
                let properties: std::collections::BTreeMap<_, _> =
                    parse_key_values(properties)?.into_iter().collect();
                let id = id.filter(|id| !id.trim().is_empty());
                client
                    .post_json(
                        &format!("/projects/{project}/tasks"),
                        &serde_json::json!({
                            "id": id,
                            "title": title,
                            "tags": tag,
                            "body": body,
                            "reason": reason,
                            "request_id": request_id,
                            "properties": properties,
                        }),
                    )
                    .await?
            }
            TaskCmd::Get { id, project } => {
                let project = manager::resolve_project(project)?;
                client
                    .get(&format!("/projects/{project}/tasks/{id}"))
                    .await?
            }
            TaskCmd::Update {
                id,
                project,
                state,
                priority,
                reason,
                request_id,
                properties,
            } => {
                let project = manager::resolve_project(project)?;
                let properties: std::collections::BTreeMap<_, _> =
                    parse_key_values(properties)?.into_iter().collect();
                let body = serde_json::json!({
                    "state": state,
                    "priority": priority,
                    "reason": reason,
                    "request_id": request_id,
                    "properties": properties,
                });
                client
                    .post_json(&format!("/projects/{project}/tasks/{id}"), &body)
                    .await?
            }
        };
        print_json(&value)
    })
}

fn cmd_id(cmd: IdCmd) -> Result<()> {
    match cmd {
        IdCmd::Mint { class } => {
            println!("{}", orgasmic_core::mint_node_id(class.into()));
            Ok(())
        }
    }
}

fn cmd_glossary(home: &Home, cmd: GlossaryCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = match cmd {
            GlossaryCmd::List { project } => {
                client
                    .get(&path_with_project_query("/glossary", project))
                    .await?
            }
            GlossaryCmd::Get { id, project } => {
                client
                    .get(&path_with_project_query(
                        &format!("/glossary/{id}"),
                        project,
                    ))
                    .await?
            }
            GlossaryCmd::Create {
                id,
                project,
                title,
                definition,
                canonical,
                avoid,
                relates_to,
                body,
                request_id,
                properties,
            } => {
                let mut properties: std::collections::BTreeMap<String, String> =
                    parse_key_values(properties)?.into_iter().collect();
                if let Some(value) = definition {
                    properties.insert("DEFINITION".to_string(), value);
                }
                if let Some(value) = canonical {
                    properties.insert("CANONICAL".to_string(), value);
                }
                if let Some(value) = avoid {
                    properties.insert("AVOID".to_string(), value);
                }
                if !relates_to.is_empty() {
                    properties.insert("RELATES_TO".to_string(), relates_to.join(" "));
                }
                let id = id.filter(|id| !id.trim().is_empty());
                client
                    .post_json(
                        "/glossary",
                        &serde_json::json!({
                            "project": project,
                            "request_id": request_id,
                            "id": id,
                            "title": title,
                            "properties": properties,
                            "body": body,
                        }),
                    )
                    .await?
            }
        };
        print_json(&value)
    })
}

fn cmd_decision(home: &Home, cmd: DecisionCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        match cmd {
            DecisionCmd::List { project } => {
                let value: serde_json::Value = client
                    .get(&path_with_project_query("/decisions", project))
                    .await?;
                print_decision_outline(&value)
            }
            DecisionCmd::Get { id, project } => {
                let value: serde_json::Value = client
                    .get(&path_with_project_query(
                        &format!("/decisions/{id}"),
                        project,
                    ))
                    .await?;
                print_decision_detail(&value)
            }
            DecisionCmd::Create {
                id,
                project,
                title,
                body,
                request_id,
                properties,
            } => {
                let properties: std::collections::BTreeMap<String, String> =
                    parse_key_values(properties)?.into_iter().collect();
                let id = id.filter(|id| !id.trim().is_empty());
                let value: serde_json::Value = client
                    .post_json(
                        "/decisions",
                        &serde_json::json!({
                            "project": project,
                            "request_id": request_id,
                            "id": id,
                            "title": title,
                            "properties": properties,
                            "body": body,
                        }),
                    )
                    .await?;
                print_json(&value)
            }
        }
    })
}

fn decision_path_key(value: &serde_json::Value) -> Vec<usize> {
    value
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .split('.')
        .filter_map(|part| part.parse::<usize>().ok())
        .collect()
}

fn print_decision_outline(value: &serde_json::Value) -> Result<()> {
    let mut rows = value
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("daemon returned non-list decision response"))?;
    rows.sort_by(|a, b| {
        decision_path_key(a)
            .cmp(&decision_path_key(b))
            .then_with(|| {
                a.get("id")
                    .and_then(serde_json::Value::as_str)
                    .cmp(&b.get("id").and_then(serde_json::Value::as_str))
            })
    });
    for row in rows {
        let id = row
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let title = row
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let path = row
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let depth = row
            .get("depth")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as usize;
        let superseded = row
            .get("superseded")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let suffix = if superseded { " [superseded]" } else { "" };
        println!("{}{} {} {}{}", "  ".repeat(depth), path, id, title, suffix);
    }
    Ok(())
}

fn print_decision_detail(value: &serde_json::Value) -> Result<()> {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    println!("{id} {title}");
    println!(
        "path: {}",
        value
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("—")
    );
    println!(
        "parent: {}",
        value
            .get("parent")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("—")
    );
    let children = value
        .get("children")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    println!(
        "children: {}",
        if children.is_empty() {
            "—"
        } else {
            children.as_str()
        }
    );
    if value
        .get("superseded")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        println!("superseded: true");
    }
    if let Some(preview) = value.get("preview").and_then(serde_json::Value::as_str) {
        if !preview.trim().is_empty() {
            println!("\n{}", preview.trim());
        }
    }
    Ok(())
}

fn cmd_architecture(home: &Home, cmd: ArchitectureCmd) -> Result<()> {
    if let ArchitectureCmd::Drift { json } = cmd {
        let cwd = std::env::current_dir().context("read current directory")?;
        let root = architecture_drift::repo_root_from(&cwd)?;
        let report = architecture_drift::run(&root)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            architecture_drift::print_human(&report);
        }
        if report.has_drift() {
            std::process::exit(1);
        }
        return Ok(());
    }
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = match cmd {
            ArchitectureCmd::List { project } => {
                client
                    .get(&path_with_project_query("/architecture", project))
                    .await?
            }
            ArchitectureCmd::Get { id, project } => {
                client
                    .get(&path_with_project_query(
                        &format!("/architecture/{id}"),
                        project,
                    ))
                    .await?
            }
            ArchitectureCmd::Drift { .. } => unreachable!("handled before daemon client"),
            ArchitectureCmd::Create {
                id,
                project,
                title,
                body,
                request_id,
                properties,
            } => {
                let properties: std::collections::BTreeMap<String, String> =
                    parse_key_values(properties)?.into_iter().collect();
                let id = id.filter(|id| !id.trim().is_empty());
                client
                    .post_json(
                        "/architecture",
                        &serde_json::json!({
                            "project": project,
                            "request_id": request_id,
                            "id": id,
                            "title": title,
                            "properties": properties,
                            "body": body,
                        }),
                    )
                    .await?
            }
        };
        print_json(&value)
    })
}

fn cmd_graph(home: &Home, cmd: GraphCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = match cmd {
            GraphCmd::Edges {
                project,
                node,
                dir,
                kind,
                relation,
            } => {
                let mut query = Vec::new();
                if let Some(project) = project.filter(|value| !value.is_empty()) {
                    query.push(format!("project={project}"));
                }
                if let Some(node) = node.filter(|value| !value.is_empty()) {
                    query.push(format!("node={node}"));
                }
                if let Some(dir) = dir.filter(|value| !value.is_empty()) {
                    query.push(format!("dir={dir}"));
                }
                if let Some(kind) = kind.filter(|value| !value.is_empty()) {
                    query.push(format!("kind={kind}"));
                }
                if let Some(relation) = relation.filter(|value| !value.is_empty()) {
                    query.push(format!("relation={relation}"));
                }
                let path = if query.is_empty() {
                    "/graph/edges".to_string()
                } else {
                    format!("/graph/edges?{}", query.join("&"))
                };
                client.get(&path).await?
            }
        };
        print_json(&value)
    })
}

fn path_with_project_query(path: &str, project: Option<String>) -> String {
    match project {
        Some(project) if !project.is_empty() => format!("{path}?project={project}"),
        _ => path.to_string(),
    }
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn cmd_recovery(home: &Home) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    let value: serde_json::Value = runtime.block_on(async {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        client.get("/recovery/status").await
    })?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn cmd_run(home: &Home, cmd: RunCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = match cmd {
            RunCmd::List => client.get("/runs").await?,
            RunCmd::Show { id } => client.get(&format!("/runs/{id}")).await?,
            RunCmd::Recover {
                id,
                action,
                project,
                request_id,
                force_inert,
            } => {
                client
                    .post_json(
                        &format!("/runs/{id}/recover"),
                        &serde_json::json!({
                            "action": action,
                            "project": project,
                            "request_id": request_id,
                            "force_inert": force_inert,
                        }),
                    )
                    .await?
            }
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    })
}

fn cmd_auth_show(home: &Home) -> Result<()> {
    let path = home.auth_token();
    if path.exists() {
        println!("[ok]   token file: {}", path.display());
        if let Ok(meta) = std::fs::metadata(&path) {
            println!("       size: {} bytes", meta.len());
        }
    } else {
        println!(
            "[warn] no token at {} — run `orgasmic status` to start the daemon and generate one",
            path.display()
        );
    }
    Ok(())
}

fn cmd_worker(home: &Home, cmd: WorkerCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let path = match cmd {
            WorkerCmd::List => "/workers".to_string(),
            WorkerCmd::Show { id } => format!("/workers/{id}"),
        };
        let value: serde_json::Value = client.get(&path).await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<(), anyhow::Error>(())
    })
}

fn cmd_prompt(home: &Home, cmd: PromptCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        match cmd {
            PromptCmd::List => {
                let value: serde_json::Value = client.get("/prompt-specs").await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            PromptCmd::Show { id } => {
                let value: serde_json::Value = client.get(&format!("/prompt-specs/{id}")).await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            PromptCmd::Compile {
                id,
                project,
                renderer,
                values,
            } => {
                let values: std::collections::BTreeMap<_, _> =
                    parse_key_values(values)?.into_iter().collect();
                let body = serde_json::json!({
                    "project": project,
                    "renderer": renderer,
                    "values": values,
                });
                let value: serde_json::Value = client
                    .post_json(&format!("/prompt-specs/{id}/compile"), &body)
                    .await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            PromptCmd::Lint {
                id,
                project,
                renderer,
                values,
            } => {
                let values: std::collections::BTreeMap<_, _> =
                    parse_key_values(values)?.into_iter().collect();
                let body = serde_json::json!({
                    "project": project,
                    "renderer": renderer,
                    "values": values,
                });
                let value: serde_json::Value = client
                    .post_json(&format!("/prompt-specs/{id}/lint"), &body)
                    .await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            PromptCmd::Fork { id } => {
                let value: serde_json::Value = client
                    .post_json(&format!("/prompt-specs/{id}/fork"), &serde_json::json!({}))
                    .await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

fn cmd_skills(home: &Home, cmd: SkillsCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let path = match cmd {
            SkillsCmd::List => "/skills".to_string(),
            SkillsCmd::Show { id } => format!("/skills/{id}"),
        };
        let value: serde_json::Value = client.get(&path).await?;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<(), anyhow::Error>(())
    })
}

fn cmd_optional(home: &Home, cmd: OptionalCmd) -> Result<()> {
    match cmd {
        OptionalCmd::List => {
            let available = content_lifecycle::list_optional(home)?;
            if available.is_empty() {
                println!("(no optional shipped content found)");
                return Ok(());
            }
            println!("{:<24} {:<16} SOURCE", "ID", "STATUS");
            for item in available {
                let status = if item.enabled { "enabled" } else { "available" };
                println!("{:<24} {:<16} {}", item.id(), status, item.source.display());
            }
            Ok(())
        }
        OptionalCmd::Enable { name } => {
            home.ensure()?;
            let entry = content_lifecycle::enable_optional(home, &name)?;
            print_lifecycle_entry("enabled optional content", &entry);
            Ok(())
        }
        OptionalCmd::Disable { name } => {
            let entry = content_lifecycle::disable_optional(home, &name)?;
            print_lifecycle_entry("disabled optional content", &entry);
            Ok(())
        }
    }
}

fn cmd_hub(home: &Home, cmd: HubCmd) -> Result<()> {
    match cmd {
        HubCmd::Install { url, family } => {
            home.ensure()?;
            let entry = content_lifecycle::install_hub(home, HubInstall { url, family })?;
            print_lifecycle_entry("installed hub content", &entry);
            Ok(())
        }
        HubCmd::List => {
            let entries = content_lifecycle::list_hub(home)?;
            if entries.is_empty() {
                println!("(no hub content installed)");
                return Ok(());
            }
            println!("{:<24} {:<14} URL", "ID", "FAMILY");
            for e in entries {
                println!(
                    "{:<24} {:<14} {}",
                    e.id(),
                    e.family,
                    e.url.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        HubCmd::Remove { name } => {
            let entry = content_lifecycle::remove_hub(home, &name)?;
            print_lifecycle_entry("removed hub content", &entry);
            Ok(())
        }
    }
}

fn print_lifecycle_entry(action: &str, entry: &LifecycleEntry) {
    println!("✓ {action}: {}", entry.id());
    if let Some(url) = &entry.url {
        println!("  url:      {url}");
    }
    println!("  target:   {}", entry.target);
    if !entry.materialized.is_empty() {
        println!("  loader:");
        for rel in &entry.materialized {
            println!("    - {rel}");
        }
    }
}

fn cmd_manager(home: &Home, cmd: ManagerCmd) -> Result<()> {
    match cmd {
        ManagerCmd::Dispatch(args) => manager::cmd_dispatch(home, args),
        ManagerCmd::DispatchClose(args) => manager::cmd_dispatch_close(home, args),
        ManagerCmd::DispatchStatus(args) => manager::cmd_dispatch_status(home, args),
        ManagerCmd::LeaseRelease(args) => manager::cmd_lease_release(home, args),
        ManagerCmd::State => {
            let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
            runtime.block_on(async move {
                let client = DaemonClient::from_home_autostart_async(home).await?;
                let value: serde_json::Value = client.get("/manager/state").await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok::<(), anyhow::Error>(())
            })
        }
    }
}

fn cmd_question(home: &Home, cmd: QuestionCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = match cmd {
            QuestionCmd::Ask { text } => {
                client
                    .post_json("/question", &serde_json::json!({ "text": text }))
                    .await?
            }
            QuestionCmd::Answer { id, text } => {
                client
                    .post_json(
                        &format!("/question/{id}/answer"),
                        &serde_json::json!({ "text": text }),
                    )
                    .await?
            }
        };
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<(), anyhow::Error>(())
    })
}

fn cmd_stage(
    home: &Home,
    stage: &str,
    project: String,
    reason: Option<String>,
    wait: bool,
) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        let value: serde_json::Value = client
            .post_json(
                &format!("/{stage}"),
                &serde_json::json!({
                    "project": project,
                    "reason": reason,
                }),
            )
            .await?;
        if wait {
            if let Some(run_id) = value.get("run_id").and_then(serde_json::Value::as_str) {
                let _ = wait_for_run_terminal(&client, run_id).await?;
            }
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok::<(), anyhow::Error>(())
    })
}

async fn wait_for_run_terminal(client: &DaemonClient, run_id: &str) -> Result<serde_json::Value> {
    loop {
        let value: serde_json::Value = client.get(&format!("/runs/{run_id}")).await?;
        if value
            .get("source")
            .and_then(serde_json::Value::as_str)
            .map(|source| source != "live")
            .unwrap_or(true)
        {
            return Ok(value);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

fn cmd_tx(home: &Home, cmd: TxCmd) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("create tokio runtime")?;
    runtime.block_on(async move {
        let client = DaemonClient::from_home_autostart_async(home).await?;
        match cmd {
            TxCmd::Record {
                ty,
                project,
                task,
                target,
                reason,
                actor,
                machine,
                request_id,
                extra,
                tx_path,
            } => {
                let extra_pairs = parse_key_values(extra)?;
                let body = serde_json::json!({
                    "request_id": request_id,
                    "type": ty,
                    "actor": actor,
                    "machine": machine,
                    "project": project,
                    "task": task,
                    "target": target,
                    "reason": reason,
                    "extra": extra_pairs,
                    "tx_path": tx_path,
                });
                let value: serde_json::Value = client.post_json("/tx", &body).await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
            TxCmd::List { project, limit } => {
                let mut path = "/tx?limit=".to_string();
                path.push_str(&limit.to_string());
                if let Some(p) = project {
                    path.push_str("&project=");
                    path.push_str(&p);
                }
                let value: serde_json::Value = client.get(&path).await?;
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
        }
        Ok::<(), anyhow::Error>(())
    })
}

fn parse_key_values(items: Vec<String>) -> Result<Vec<(String, String)>> {
    items
        .into_iter()
        .map(|kv| -> Result<(String, String)> {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("expected KEY=VALUE, got `{kv}`"))?;
            Ok((k.to_string(), v.to_string()))
        })
        .collect::<Result<_>>()
}
