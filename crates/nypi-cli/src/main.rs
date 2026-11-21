//! `nypi`: the NyPi command-line agent.

#[cfg(test)]
mod admin_tests;
mod board;
mod client;
mod dmn;
mod eval;
mod goals;
mod machine;
mod memory;
mod node;
mod portrait;
mod rate;
mod reflections;
mod render;
mod run_once;
mod scorecard;
mod secrets;
mod specialists;
mod supervisor;
mod tasks;
mod trace;
mod trust;
mod tui;
mod update;
#[cfg(test)]
mod update_tests;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use nypi_ai::ThinkingLevel;
use nypi_core::config::Paths;
use nypi_core::runtime::{Job, Runtime, RuntimeOptions, SessionMode};
// The hub's modules, named here as the rest of this crate names them.
use nypi_hub::channels::telegram;
use nypi_hub::{bridge, commands, daemon, jobs, notify, rpc, scheduler, watchdog};

use crate::run_once::print_mode;

/// `nypi --version`: `nypi <version> (<target> <commit>)`; a new binary is checked by it before
/// it replaces the old one (decision D98).
const VERSION_LINE: &str =
    concat!(env!("CARGO_PKG_VERSION"), " (", env!("NYPI_TARGET"), " ", env!("NYPI_GIT_SHA"), ")");

#[derive(Parser)]
#[command(
    name = "nypi",
    version = VERSION_LINE,
    about = "NyPi: a compiled, extensible personal agent",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Prompt text. With --print, run it and exit; otherwise it is the first interactive message.
    prompt: Vec<String>,
    /// Run the prompt non-interactively and print the final answer.
    #[arg(short, long)]
    print: bool,
    /// Emit every event as a JSON line (implies --print).
    #[arg(long)]
    json: bool,
    /// Output mode, as in Pi: `rpc` (same as the rpc subcommand), `json`, or `text`.
    #[arg(long)]
    mode: Option<String>,
    #[command(flatten)]
    common: CommonArgs,
}

#[derive(clap::Args, Clone)]
struct CommonArgs {
    /// Model as provider/id (or a bare id).
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Thinking level: off, minimal, low, medium, high, xhigh, max.
    #[arg(short, long, global = true)]
    thinking: Option<String>,
    /// Models to cycle through, as Pi's --models: comma-separated provider/id, ids, or globs, each with an
    /// optional :level. Replaces the enabledModels setting; a new session starts on one of them.
    #[arg(long, global = true, value_delimiter = ',')]
    models: Option<Vec<String>>,
    /// Comma-separated tool names (read,bash,edit,write,grep,find,ls).
    #[arg(long, global = true, value_delimiter = ',')]
    tools: Option<Vec<String>>,
    /// Continue the most recent session in this directory.
    #[arg(short = 'c', long = "continue", global = true)]
    continue_session: bool,
    /// Open a specific session file.
    #[arg(long, global = true)]
    session: Option<PathBuf>,
    /// Do not persist this session.
    #[arg(long, global = true)]
    no_session: bool,
    /// Hide model thinking output.
    #[arg(long, global = true)]
    hide_thinking: bool,
    /// Put a fast chat module on this model in front of the worker.
    #[arg(long, global = true)]
    chat_model: Option<String>,
    /// One agent does everything, even when settings name a chat model.
    #[arg(long, global = true)]
    no_chat: bool,
    /// Full-screen terminal UI with a live task panel (or set "tui": true in settings).
    #[arg(long, global = true)]
    tui: bool,
    /// Line-based interactive mode, even when settings ask for the full-screen UI.
    #[arg(long, global = true)]
    no_tui: bool,
    /// Nobody is watching this run (cron or launchd started it): its result, or its failure to
    /// start, always goes to the user through the outbox.
    #[arg(long, global = true, hide = true)]
    unattended: bool,
    /// Run with a session of its own in this process, even when the daemon is running.
    #[arg(long, global = true)]
    standalone: bool,
    /// Work on another device: the conversation is the daemon's, and its files and shell are that
    /// device's node's (nypi devices lists them).
    #[arg(long, global = true, value_name = "DEVICE")]
    on: Option<String>,
    /// With --on: the folder on that device (default: this folder's path).
    #[arg(long, global = true, value_name = "FOLDER", requires = "on")]
    at: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Telegram is a channel of the daemon (it needs a bot token: channels.telegram.botTokenSecret
    /// or TELEGRAM_BOT_TOKEN): how to pair a chat when the daemon runs, and otherwise the daemon
    /// itself, here in the foreground, with the channel on.
    Telegram,
    /// Let another program drive the agent: JSONL commands on stdin, responses and events on stdout.
    Rpc,
    /// List known models, optionally filtered.
    Models { filter: Option<String> },
    /// List sessions for the current directory.
    Sessions,
    /// Lucia's long-term memory: the newest memories, search, add, or forget.
    Memory {
        #[command(subcommand)]
        action: Option<MemoryCommand>,
    },
    /// Lucia's idle-time reflection (the DMN): whether a pass is due, recent passes, or run one now.
    Dmn {
        #[command(subcommand)]
        action: Option<DmnCommand>,
    },
    /// Timers and condition triggers: list them, add one, cancel one, or check once now.
    Schedule {
        #[command(subcommand)]
        action: Option<ScheduleCommand>,
        /// Include finished ones.
        #[arg(long)]
        all: bool,
    },
    /// What Lucia has learned about you: traits with the words behind them, or forget one.
    Portrait {
        #[command(subcommand)]
        action: Option<PortraitCommand>,
    },
    /// Run the offline benchmark: tasks with one hurdle each, in a temporary home and folder,
    /// judged by what was really done. Uses your model (`--model` picks another) and so costs
    /// what its requests cost.
    Eval {
        /// Run only this scenario (repeatable); all of the suite without it.
        #[arg(long = "scenario")]
        scenarios: Vec<String>,
        /// List the scenarios of every suite and what stands in the way in each.
        #[arg(long)]
        list: bool,
        /// Print the report as JSON.
        #[arg(long = "json")]
        as_json: bool,
        /// Run each scenario through chat, the Principal, and the task pool instead of a worker
        /// alone. The chat model is `--chat-model`, else your settings' `chat.model`, else the
        /// worker's; the Principal's likewise.
        #[arg(long)]
        three_planes: bool,
        /// The Principal's model on a three-plane run (`provider/id`).
        #[arg(long, value_name = "MODEL")]
        principal_model: Option<String>,
        /// Which scenarios: `worker` (the twelve hurdles for one worker), `planes` (the four of
        /// the three planes' way of working), or `all`. Default: `worker`, and `all` with
        /// `--three-planes`.
        #[arg(long, value_name = "SUITE")]
        suite: Option<String>,
    },
    /// The secrets Lucia may use but never sees: store one under a handle, list the handles,
    /// or remove one.
    Secrets {
        #[command(subcommand)]
        action: SecretsCommand,
    },
    /// Check the notification channels: list them and send a test notice.
    Notify {
        #[command(subcommand)]
        action: NotifyCommand,
    },
    /// Recent post-task reflections: lessons learned and skill proposals to review.
    Reflections {
        /// How many to list.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Show or edit the goals Lucia keeps advancing, or run a goal review now.
    Goals {
        #[command(subcommand)]
        action: Option<GoalsCommand>,
    },
    /// List specialists, show or create one, or give one a task.
    Specialists {
        #[command(subcommand)]
        action: Option<SpecialistsCommand>,
    },
    /// List tasks from the ledger, show one, or resume an interrupted one.
    Tasks {
        #[command(subcommand)]
        action: Option<TasksCommand>,
        /// Include finished tasks.
        #[arg(long)]
        all: bool,
    },
    /// Say how a task's result was: good, fixed (you had to push or correct), wrong, or silent
    /// (it stopped without telling you). With nothing named, go through the tasks without a
    /// verdict, one key each.
    Rate {
        /// The task (nypi tasks --all lists them).
        task: Option<String>,
        /// good, fixed, wrong, or silent (or g, f, w, s).
        verdict: Option<String>,
        /// Why, in a few words.
        note: Vec<String>,
        /// With nothing named: how many days back to go.
        #[arg(long, default_value_t = rate::REVIEW_DAYS)]
        days: i64,
    },
    /// How NyPi served you this week, against your bars, from its own records and your verdicts.
    Scorecard {
        /// How many weeks to show, this one included.
        #[arg(long, default_value_t = 4)]
        weeks: u32,
        /// Print the numbers as JSON.
        #[arg(long = "json")]
        as_json: bool,
    },
    /// Check the task ledger once: flag stalled tasks, resume interrupted ones, fail hopeless ones.
    Watchdog,
    /// Give the daemon a message and print the result. While she is working, it steers her.
    Send {
        text: Vec<String>,
        /// Return once the message is taken; the result reaches you as a notice.
        #[arg(long)]
        no_wait: bool,
    },
    /// Talk with the daemon: see what she is doing, steer, approve, stop. Leaving does not stop her.
    Attach,
    /// This machine as the hub (decision D93): `up` sets it up and keeps it running, `run`
    /// runs it in the foreground (what the supervisor starts), `down` stops it and keeps its data.
    Hub {
        #[command(subcommand)]
        action: HubAction,
    },
    /// This machine as a node of a hub: `up` asks the hub to admit it (with a six-digit code
    /// you confirm on a machine you already trust) and keeps it running; `run` runs it in the
    /// foreground; `down` stops it; `leave` unpairs it.
    Node {
        #[command(subcommand)]
        action: NodeAction,
    },
    /// The relay behind the entry (decision D94): `up` sets it up beside whatever this machine
    /// is, `run` runs it in the foreground, `down` stops it.
    Relay {
        #[command(subcommand)]
        action: RelayAction,
    },
    /// Update NyPi from the public repository's signed releases (decision D98): this machine,
    /// one node, or every machine of the hub in turn.
    Update {
        #[command(subcommand)]
        action: Option<UpdateAction>,
        /// Only say whether a newer release exists.
        #[arg(long)]
        check: bool,
        /// This version (vX.Y.Z), older ones too; else the latest, when it is newer.
        #[arg(long)]
        version: Option<String>,
        /// Do not ask.
        #[arg(long)]
        yes: bool,
        /// Update the connected node of this name instead of this machine.
        #[arg(long)]
        node: Option<String>,
        /// Update every machine of the hub: the nodes one by one, then the hub.
        #[arg(long)]
        all: bool,
        /// With --all, also the machines that run the relay.
        #[arg(long)]
        include_relay: bool,
        /// A development build without a release key: take an unsigned manifest (by hand only).
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// What this machine is (hub or node), whether its service runs, and how it is linked.
    Status,
    /// The machines and browsers admitted to the hub, and the requests waiting for an answer.
    Devices {
        #[command(subcommand)]
        action: Option<DevicesAction>,
    },
    /// The service that keeps this machine's hub or node running (started by `hub up` and
    /// `node up`; not run by hand).
    #[command(hide = true)]
    Supervisor,
    /// Whether this folder's own NyPi configuration is loaded: show it, or decide.
    Trust {
        #[command(subcommand)]
        action: Option<TrustCommand>,
    },
    /// What NyPi tried to tell you while you were away, and whether it arrived.
    Notices {
        /// Include the ones that reached you.
        #[arg(long)]
        all: bool,
    },
    /// Run the board: a web page showing what needs you, what runs now, and what happened,
    /// with approve, deny, stop, and message. Reachable from this network with the token it prints.
    Board {
        /// Keep the page on this machine (127.0.0.1) instead of the local network.
        #[arg(long)]
        local: bool,
        #[arg(long, default_value_t = 7878)]
        port: u16,
        /// Make a new token; links with the old one stop working.
        #[arg(long)]
        new_token: bool,
        /// Print the addresses of the board that is already running, and exit.
        #[arg(long)]
        link: bool,
    },
    /// Show recent pulses, or the causal chain and effects of one pulse, from the audit log.
    Trace {
        /// Pulse ID (RPC `agent_settled.pulse`, or from the list).
        id: Option<String>,
        /// How many days of audit log to search.
        #[arg(long, default_value_t = 7)]
        days: usize,
    },
}

#[derive(Subcommand)]
enum TrustCommand {
    /// Load this folder's NyPi configuration from now on.
    Yes,
    /// Trust the parent folder, and with it everything under it.
    Parent,
    /// Never load this folder's NyPi configuration, and stop asking.
    No,
    /// Forget the decision for this folder.
    Forget,
}

#[derive(Subcommand)]
enum HubAction {
    /// Make this machine the hub, or start it again if it is stopped. Shows join requests
    /// until Ctrl-C (the hub keeps running).
    Up {
        /// The entry machines first find the hub at (`host:port`, `https://…`, or `http://host:port`).
        #[arg(long)]
        entry: Option<String>,
        /// What the hub's own machine is called.
        #[arg(long)]
        name: Option<String>,
    },
    /// Move the hub to this machine (a personal node): the hub stops its work, sends what is new
    /// since this machine's standby copy, and becomes a node of this one.
    Here {
        /// The hub is gone: take over from the standby copy, losing what it did after the copy.
        #[arg(long)]
        force: bool,
        /// With --force: do not ask.
        #[arg(long)]
        yes: bool,
    },
    /// Run the hub here in the foreground: Lucia's conversation, the place machines link to.
    Run,
    /// Stop the hub and keep all its data.
    Down,
}

#[derive(Subcommand)]
enum UpdateAction {
    /// Put the previous binary back.
    Rollback,
    /// Make a release signing key: the private half to a file, the public half printed.
    Keygen {
        #[arg(long, default_value = "nypi-release.key")]
        out: PathBuf,
    },
    /// Sign a release manifest: writes <manifest>.sig.
    Sign {
        manifest: PathBuf,
        /// The private key's file; else NYPI_RELEASE_SIGNING_KEY.
        #[arg(long)]
        key: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum RelayAction {
    /// Run the relay here, kept by the supervisor.
    Up {
        /// The fingerprint of the only hub key the relay takes (what nypi hub up showed).
        #[arg(long)]
        hub_key: Option<String>,
        /// Where it listens (default 127.0.0.1:7682, behind Cloudflare Tunnel or a reverse proxy).
        #[arg(long)]
        listen: Option<String>,
    },
    /// Run the relay here in the foreground.
    Run,
    /// Stop the relay.
    Down,
}

#[derive(Subcommand)]
enum NodeAction {
    /// Join the hub and keep this machine linked to it.
    Up {
        /// What the hub calls this machine (default: its host name).
        #[arg(long)]
        name: Option<String>,
        /// An invite code (`nypi devices invite` on the hub): admitted with no question.
        #[arg(long)]
        code: Option<String>,
        /// The entry the hub is found at (`host:port`).
        #[arg(long)]
        entry: Option<String>,
    },
    /// Run the node here in the foreground: carry out the hub's calls, under this machine's rules.
    Run,
    /// Stop the node and keep its data.
    Down,
    /// Unpair this machine: tell the hub, and forget its key and the hub.
    Leave,
}

#[derive(Subcommand)]
enum DevicesAction {
    /// Join requests waiting for an answer.
    Pending,
    /// Admit the request with this code.
    Admit {
        code: String,
        /// It carries out calls only, and cannot talk to the hub.
        #[arg(long)]
        exec_only: bool,
    },
    /// Refuse the request with this code.
    Refuse { code: String },
    /// Unpair the machine or browser of this name; its open links close at once.
    Remove { name: String },
    /// A one-time code for a machine nobody watches: `nypi node up --code <code>` admits it.
    Invite {
        name: String,
        #[arg(long)]
        exec_only: bool,
        /// How long it works: `30m`, `1h`, `2d`.
        #[arg(long, default_value = "1h")]
        expires: String,
    },
}

#[derive(Subcommand)]
enum DmnCommand {
    /// Run one pass now, whatever the schedule says.
    Run {
        /// Why this pass runs (the scheduler says idle or interval).
        #[arg(long, default_value = "manual")]
        why: String,
    },
}

#[derive(Subcommand)]
enum ScheduleCommand {
    /// Add one: exactly one trigger and one action.
    Add {
        /// Once, at local "YYYY-MM-DD HH:MM" (or RFC 3339).
        #[arg(long)]
        at: Option<String>,
        /// Every this many minutes.
        #[arg(long)]
        every: Option<u64>,
        /// Every day at HH:MM.
        #[arg(long)]
        daily: Option<String>,
        /// With --daily: only these days (mon,tue,...).
        #[arg(long, value_delimiter = ',')]
        weekdays: Vec<String>,
        /// When this file changes.
        #[arg(long)]
        when_file: Option<String>,
        /// When this command succeeds (or, with --changes, when its output changes).
        #[arg(long)]
        when_command: Option<String>,
        #[arg(long)]
        changes: bool,
        /// Fire a condition every time, not once.
        #[arg(long)]
        repeat: bool,
        /// Send this notice.
        #[arg(long)]
        notify: Option<String>,
        /// Run this as a task for Lucia.
        #[arg(long)]
        prompt: Option<String>,
    },
    Cancel {
        id: String,
    },
    /// Fire whatever is due now and carry it out (for launchd or cron, when no hub runs).
    Tick,
}

#[derive(Subcommand)]
enum SecretsCommand {
    /// Store a secret as <scope>/<name> (for example github/lucia-bot). The value is typed
    /// unseen, or piped in; it is never an argument.
    Set { name: String },
    /// The handles and when each was set. Never a value.
    List,
    /// Remove a secret.
    Rm { name: String },
}

#[derive(Subcommand)]
enum PortraitCommand {
    /// A trait and the words of yours it rests on.
    Show { id: String },
    /// Remove a trait.
    Forget { id: String },
}

#[derive(Subcommand)]
enum ModelCommand {
    /// Fetch the local embedding model (about 400 MB) and verify its checksum.
    Download,
}

#[derive(Subcommand)]
enum MemoryCommand {
    Search {
        query: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Show the factors behind each score: relevance, importance, emotion, activation.
        #[arg(long)]
        explain: bool,
    },
    Add {
        text: Vec<String>,
        /// Comma-separated tags that help find it (synonyms, other languages, names).
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// fact, preference, decision, event, person, or commitment.
        #[arg(long, default_value = "fact")]
        kind: String,
    },
    /// Give every memory a vector from the active embedding model.
    Reembed,
    /// The embedding sources, or `download` to fetch the local model.
    Model {
        #[command(subcommand)]
        action: Option<ModelCommand>,
    },
    Forget {
        id: String,
    },
}

#[derive(Subcommand)]
enum NotifyCommand {
    /// Send a test notice to every configured channel and report which ones worked.
    Test { text: Vec<String> },
}

#[derive(Subcommand)]
enum GoalsCommand {
    /// Add a goal.
    Add {
        text: Vec<String>,
        /// short, medium, or long.
        #[arg(long, default_value = "short")]
        term: String,
        #[arg(long)]
        next: Option<String>,
        /// YYYY-MM-DD.
        #[arg(long)]
        due: Option<String>,
        /// The project folder the goal is about.
        #[arg(long)]
        dir: Option<String>,
    },
    Done {
        id: String,
    },
    Reopen {
        id: String,
    },
    /// Let Lucia advance the most valuable goal she can move without you, then report.
    Review,
}

#[derive(Subcommand)]
enum SpecialistsCommand {
    /// A specialist's profile, instructions, and recent experience.
    Show { name: String },
    /// Find installed agent CLIs (Codex, Claude Code) and whether they are ready to work.
    Detect,
    /// Create a specialist in ~/.nypi/specialists from a template: native, codex, or claude.
    New {
        name: String,
        #[arg(long, default_value = "native")]
        from: String,
    },
    /// Give a specialist a task in this directory and print its answer.
    Run { name: String, task: Vec<String> },
}

#[derive(Subcommand)]
enum TasksCommand {
    /// Everything the ledger knows about one task.
    Show { id: String },
    /// Continue an interrupted task in its session and working directory, then print the result.
    Resume { id: String },
    /// Close a task that will not continue (it no longer counts as open).
    Cancel { id: String },
}

/// Set once at startup when nobody is watching this process (`--unattended`).
static UNATTENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn unattended() -> bool {
    UNATTENDED.load(std::sync::atomic::Ordering::Relaxed)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    nypi_hub::protocol::set_admin_runner(admin_runner());
    UNATTENDED.store(cli.common.unattended, std::sync::atomic::Ordering::Relaxed);
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(run(cli));
    if let (true, Err(error)) = (unattended(), &result) {
        runtime.block_on(report_unattended_failure(error));
    }
    std::process::exit(result?);
}

/// The hub's own commands (decision D96): those that work on the hub's stores. They run here
/// when this machine is the hub (or neither); on a personal node they are carried to the hub,
/// and on an exec-only node they are refused. None when `cli` names none of them.
async fn route_hub_command(cli: &Cli, cwd: &std::path::Path) -> Option<anyhow::Result<i32>> {
    if !is_hub_command(cli) {
        return None;
    }
    let paths = Paths::discover();
    let node = nypi_hub::instance::Instance::load(&paths).filter(|instance| !instance.is_hub());
    let argv = hub_argv();
    let input_needed = matches!(&cli.command, Some(Command::Secrets { action: SecretsCommand::Set { .. } }));
    let Some(node) = node.filter(|_| nypi_hub::protocol::admin_allowed(&argv, input_needed).is_ok()) else {
        // `update --node` and `--all` need the daemon's register of nodes: asked of it.
        if matches!(&cli.command, Some(Command::Update { .. })) {
            return Some(client::hub_command_on_hub(&paths, argv).await);
        }
        return Some(hub_command(&paths, cwd, cli, None, None).await);
    };
    if node.trust != nypi_hub::pairing::Trust::Personal {
        eprintln!(
            "This machine was admitted as exec-only: nypi {} works on the hub's stores, from the hub or the user's own \
             machines.",
            argv.first().cloned().unwrap_or_default()
        );
        return Some(Ok(1));
    }
    Some(client::hub_command_here(&paths, cli_rate_walk(cli), argv, input_needed).await)
}

/// The days a `nypi rate` walk-through covers, when that is what `cli` asks for.
fn cli_rate_walk(cli: &Cli) -> Option<i64> {
    match &cli.command {
        Some(Command::Rate { task: None, days, .. }) => Some(*days),
        _ => None,
    }
}

/// The words after `nypi`, from the hub command's name on.
fn hub_argv() -> Vec<String> {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let start = words.iter().position(|word| nypi_hub::protocol::ADMIN_COMMANDS.contains(&word.as_str())).unwrap_or(0);
    words[start..].to_vec()
}

/// Whether `cli` is one of the commands `hub_command` runs.
fn is_hub_command(cli: &Cli) -> bool {
    matches!(
        &cli.command,
        Some(
            Command::Trace { .. }
                | Command::Rate { .. }
                | Command::Scorecard { .. }
                | Command::Memory {
                    action: None
                        | Some(
                            MemoryCommand::Search { .. }
                                | MemoryCommand::Add { .. }
                                | MemoryCommand::Forget { .. }
                                | MemoryCommand::Reembed
                        )
                }
                | Command::Dmn { action: None }
                | Command::Schedule { .. }
                | Command::Eval { list: true, .. }
                | Command::Secrets { .. }
                | Command::Portrait { .. }
                | Command::Reflections { .. }
                | Command::Specialists { .. }
                | Command::Devices { .. }
                | Command::Notices { .. }
        )
    ) || matches!(&cli.command, Some(Command::Update { action: None, node: Some(_), .. }))
        || matches!(&cli.command, Some(Command::Update { action: None, all: true, .. }))
        || matches!(
            &cli.command,
            Some(Command::Tasks { action: None | Some(TasksCommand::Show { .. } | TasksCommand::Cancel { .. }), .. })
        )
        || matches!(&cli.command, Some(Command::Goals { action }) if !matches!(action, Some(GoalsCommand::Review)))
}

/// Run one of the hub's commands with the NyPi home `paths`: in a terminal on the hub, or for a
/// personal machine, its text kept (`nypi_hub::output`). `input` is what it would read from a
/// terminal (the value of `secrets set`).
async fn hub_command(
    paths: &Paths,
    cwd: &std::path::Path,
    cli: &Cli,
    input: Option<String>,
    nodes: Option<&nypi_hub::nodes::Nodes>,
) -> anyhow::Result<i32> {
    match &cli.command {
        // Run in the daemon, which holds the register of nodes (decision D98).
        Some(Command::Update { node: Some(name), version, .. }) => {
            let nodes = nodes.ok_or_else(|| anyhow::anyhow!("the hub is not running"))?;
            let said = update::update_node(nodes, name, version.clone()).await?;
            nypi_hub::outln!("{name}: {said}");
            return Ok(0);
        }
        Some(Command::Update { all: true, version, include_relay, .. }) => {
            let nodes = nodes.ok_or_else(|| anyhow::anyhow!("the hub is not running"))?;
            return update::update_all(paths, nodes, version.clone(), *include_relay).await;
        }
        Some(Command::Trace { id, days }) => {
            return trace::run(paths, id.as_deref(), *days);
        }
        Some(Command::Tasks { action: None, all }) => return tasks::list(paths, *all),
        Some(Command::Tasks { action: Some(TasksCommand::Show { id }), .. }) => {
            return tasks::show(paths, id);
        }
        Some(Command::Tasks { action: Some(TasksCommand::Cancel { id }), .. }) => {
            return tasks::cancel(paths, id);
        }
        Some(Command::Rate { task, verdict, note, days }) => {
            let paths = paths.clone();
            return match (task, verdict) {
                (None, _) => rate::review(&paths, *days),
                (Some(task), Some(verdict)) => rate::rate(&paths, task, verdict, &note.join(" ")),
                (Some(task), None) => {
                    eprintln!(
                        "Say how it was: nypi rate {task} <verdict> [note]   ({})",
                        nypi_core::ledger::Verdict::choices()
                    );
                    Ok(1)
                }
            };
        }
        Some(Command::Scorecard { weeks, as_json }) => {
            let paths = paths.clone();
            let settings = nypi_core::config::Settings::load(&paths, cwd)?;
            return scorecard::run(&paths, &settings, *weeks, *as_json);
        }
        Some(Command::Memory { action }) => {
            let paths = paths.clone();
            let settings = nypi_core::config::Settings::load(&paths, cwd)?;
            // The embedding key, if any, lives in the env files.
            nypi_ai::auth::load_env_file(&paths.env_file());
            for file in &settings.env_files {
                nypi_ai::auth::load_env_file(&nypi_core::config::expand_home(file));
            }
            return match action {
                None => memory::list(&paths, &settings, 20),
                Some(MemoryCommand::Search { query, limit, explain }) => {
                    memory::search(&paths, &settings, &query.join(" "), *limit, *explain).await
                }
                Some(MemoryCommand::Add { text, tags, kind }) => {
                    memory::add(&paths, &settings, &text.join(" "), tags.clone(), kind).await
                }
                Some(MemoryCommand::Forget { id }) => memory::forget(&paths, id),
                Some(MemoryCommand::Reembed) => memory::reembed(&paths, &settings).await,
                // This machine's own embedding model: run in `run`, never for another machine.
                Some(MemoryCommand::Model { .. }) => anyhow::bail!("`nypi memory model` is about this machine"),
            };
        }
        Some(Command::Dmn { action: None }) => {
            let paths = paths.clone();
            let settings = nypi_core::config::Settings::load(&paths, cwd)?;
            return dmn::status(&paths, &settings);
        }
        Some(Command::Schedule { action, all }) => {
            let paths = paths.clone();
            return match action {
                None => scheduler::list(&paths, *all),
                Some(ScheduleCommand::Add {
                    at,
                    every,
                    daily,
                    weekdays,
                    when_file,
                    when_command,
                    changes,
                    repeat,
                    notify,
                    prompt,
                }) => scheduler::add(
                    &paths,
                    cwd,
                    scheduler::AddArgs {
                        at: at.clone(),
                        every: *every,
                        daily: daily.clone(),
                        weekdays: weekdays.clone(),
                        when_file: when_file.clone(),
                        when_command: when_command.clone(),
                        changes: *changes,
                        notify: notify.clone(),
                        prompt: prompt.clone(),
                        repeat: *repeat,
                    },
                ),
                Some(ScheduleCommand::Cancel { id }) => scheduler::cancel(&paths, id),
                Some(ScheduleCommand::Tick) => {
                    let settings = nypi_core::config::Settings::load(&paths, cwd)?;
                    nypi_ai::auth::load_env_file(&paths.env_file());
                    for file in &settings.env_files {
                        nypi_ai::auth::load_env_file(&nypi_core::config::expand_home(file));
                    }
                    let notifier = notify::Notifier::from_settings(&settings, &paths, cwd, true);
                    scheduler::tick(&paths, &settings.dmn, notifier).await
                }
            };
        }
        Some(Command::Eval { list: true, .. }) => return eval::list(),
        Some(Command::Secrets { action }) => {
            let paths = paths.clone();
            return match action {
                SecretsCommand::Set { name } => match input {
                    // Read on the personal machine that asked, and sent over its link (D96).
                    Some(value) => secrets::set_value(&paths, name, &value),
                    None => secrets::set(&paths, name),
                },
                SecretsCommand::List => secrets::list(&paths),
                SecretsCommand::Rm { name } => secrets::remove(&paths, name),
            };
        }
        Some(Command::Portrait { action }) => {
            let paths = paths.clone();
            return match action {
                None => portrait::list(&paths),
                Some(PortraitCommand::Show { id }) => portrait::show(&paths, id),
                Some(PortraitCommand::Forget { id }) => portrait::forget(&paths, id),
            };
        }
        Some(Command::Reflections { limit }) => {
            return reflections::list(paths, *limit);
        }
        Some(Command::Goals { action }) => {
            let paths = paths.clone();
            match action {
                None => return goals::show(&paths),
                Some(GoalsCommand::Add { text, term, next, due, dir }) => {
                    return goals::add(&paths, term, &text.join(" "), next.clone(), due.clone(), dir.clone());
                }
                Some(GoalsCommand::Done { id }) => return goals::set_done(&paths, id, true),
                Some(GoalsCommand::Reopen { id }) => return goals::set_done(&paths, id, false),
                // A review is a normal run; it continues below.
                Some(GoalsCommand::Review) => {}
            }
        }
        Some(Command::Specialists { action }) => {
            let paths = paths.clone();
            return match action {
                None => specialists::list(&paths, cwd),
                Some(SpecialistsCommand::Show { name }) => specialists::show(&paths, cwd, name),
                Some(SpecialistsCommand::Detect) => specialists::detect(&paths, cwd),
                Some(SpecialistsCommand::New { name, from }) => specialists::new(&paths, name, from),
                Some(SpecialistsCommand::Run { name, task }) => {
                    specialists::run(&paths, cwd, name, &task.join(" "), cli.json).await
                }
            };
        }
        Some(Command::Devices { action }) => {
            let paths = paths.clone();
            return match action {
                None => machine::devices(&paths),
                Some(DevicesAction::Pending) => machine::pending(&paths),
                Some(DevicesAction::Admit { code, exec_only }) => machine::answer(&paths, code, Some(*exec_only)),
                Some(DevicesAction::Refuse { code }) => machine::answer(&paths, code, None),
                Some(DevicesAction::Remove { name }) => machine::remove(&paths, name).await,
                Some(DevicesAction::Invite { name, exec_only, expires }) => {
                    machine::invite(&paths, name, *exec_only, expires)
                }
            };
        }
        Some(Command::Notices { all }) => return notify::list(paths, *all),
        _ => {}
    }
    anyhow::bail!("not one of the hub's commands")
}

/// How the hub runs a command for a personal machine (decision D96): parsed as this program
/// parses its own command line, run by `hub_command` with the hub's home, its text kept.
fn admin_runner() -> nypi_hub::protocol::AdminRunner {
    std::sync::Arc::new(|paths, argv, input, context| {
        Box::pin(async move {
            let cli = match Cli::try_parse_from(std::iter::once("nypi".to_string()).chain(argv)) {
                Ok(cli) => cli,
                Err(error) => return (error.to_string(), 2),
            };
            if !is_hub_command(&cli) {
                return ("not one of the hub's commands".into(), 2);
            }
            let cwd = dirs::home_dir().unwrap_or_else(|| paths.home.clone());
            let input = input.map(|hidden| hidden.0);
            let nodes = context.nodes;
            let (result, mut text) =
                nypi_hub::output::capture(hub_command(&paths, &cwd, &cli, input, nodes.as_deref())).await;
            match result {
                Ok(code) => (text, code),
                Err(error) => {
                    text.push_str(&format!("Error: {error:#}\n"));
                    (text, 1)
                }
            }
        })
    })
}

/// An unattended run that could not start or finish has nobody to read its error (principle
/// B6): it goes to the outbox and the channels, with whatever settings can still be read.
async fn report_unattended_failure(error: &anyhow::Error) {
    let paths = Paths::discover();
    let cwd = std::env::current_dir().unwrap_or_else(|_| paths.home.clone());
    let settings = nypi_core::config::Settings::load(&paths, &cwd).unwrap_or_default();
    nypi_ai::auth::load_env_file(&paths.env_file());
    for file in &settings.env_files {
        nypi_ai::auth::load_env_file(&nypi_core::config::expand_home(file));
    }
    // The arguments name a task or a schedule by id; a prompt is never among them.
    let what: Vec<String> = std::env::args().skip(1).filter(|a| a != "--unattended").collect();
    let text = format!("Failed: a background run could not finish (nypi {}): {error:#}.", what.join(" "));
    let notifier = notify::Notifier::from_settings(&settings, &paths, &cwd, true);
    let level = nypi_core::inspector::AlertLevel::Error;
    let notice = notify::Notice { source: "run", level, text: &text, task: None, shown_in: None };
    notify::post(&paths, notifier.as_ref(), notice).await;
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let mut cli = cli;
    match cli.mode.as_deref() {
        None | Some("text") => {}
        Some("json") => cli.json = true,
        Some("rpc") => cli.command = Some(Command::Rpc),
        Some(other) => anyhow::bail!("Unknown mode: {other} (expected text, json, or rpc)"),
    }
    if let Some(result) = route_hub_command(&cli, &cwd).await {
        return result;
    }
    match &cli.command {
        Some(Command::Memory { action: Some(MemoryCommand::Model { action }) }) => {
            let paths = Paths::discover();
            let settings = nypi_core::config::Settings::load(&paths, &cwd)?;
            return match action {
                None => memory::model_status(&paths, &settings),
                Some(ModelCommand::Download) => memory::model_download(&paths, &settings).await,
            };
        }
        Some(Command::Models { filter }) => {
            let registry = nypi_ai::ModelRegistry::load(&Paths::discover().models())?;
            commands::print_models(&registry, filter.as_deref(), usize::MAX);
            return Ok(0);
        }
        Some(Command::Sessions) => {
            commands::print_sessions(&Paths::discover(), &cwd, 50);
            return Ok(0);
        }
        // With the daemon running Telegram as a channel there is no bridge to start: say how to pair.
        Some(Command::Telegram) if let Some(help) = telegram::pairing_help(&Paths::discover()) => {
            println!("{help}");
            return Ok(0);
        }
        Some(Command::Board { link: true, .. }) => return board::print_link(&Paths::discover()),
        Some(Command::Board { local, port, new_token, .. }) => {
            return board::run(&Paths::discover(), *local, *port, *new_token).await;
        }
        Some(Command::Eval { scenarios, as_json, three_planes, principal_model, suite, .. }) => {
            let options = eval::Options {
                model: cli.common.model.clone(),
                three_planes: *three_planes,
                chat_model: cli.common.chat_model.clone(),
                principal_model: principal_model.clone(),
                suite: suite.clone(),
            };
            return eval::run(&Paths::discover(), scenarios, options, *as_json).await;
        }
        Some(Command::Notify { action: NotifyCommand::Test { text } }) => {
            let paths = Paths::discover();
            let settings = nypi_core::config::Settings::load(&paths, &cwd)?;
            // Env files hold webhook secrets and the bot token.
            nypi_ai::auth::load_env_file(&paths.env_file());
            for file in &settings.env_files {
                nypi_ai::auth::load_env_file(&nypi_core::config::expand_home(file));
            }
            let text = if text.is_empty() { "This is a test notice from NyPi.".to_string() } else { text.join(" ") };
            return Ok(notify::test(&settings, &paths, &cwd, &text).await);
        }
        Some(Command::Watchdog) => {
            let paths = Paths::discover();
            let settings = nypi_core::config::Settings::load(&paths, &cwd)?;
            nypi_ai::auth::load_env_file(&paths.env_file());
            for file in &settings.env_files {
                nypi_ai::auth::load_env_file(&nypi_core::config::expand_home(file));
            }
            let notifier = notify::Notifier::from_settings(&settings, &paths, &cwd, true);
            let round = watchdog::run_once(&paths, &settings.watchdog).await;
            if round.reports.is_empty() {
                println!("Nothing needs attention (or another NyPi process is checking right now).");
            }
            // The round has already acted on what it found (a task marked stalled or failed is
            // not found again), so printing alone would lose it when cron runs this.
            let shown_in = std::io::stdout().is_terminal().then_some("the terminal");
            for (level, message) in round.reports {
                println!("{message}");
                let notice = notify::Notice { source: "watchdog", level, text: &message, task: None, shown_in };
                notify::post(&paths, notifier.as_ref(), notice).await;
            }
            notify::flush(&paths, notifier.as_ref()).await;
            // A task taken up again runs here, and the command returns when it has ended.
            let jobs = jobs::Jobs::here(&paths, nypi_core::pulse::PulseBus::new());
            round.orders.into_iter().for_each(|order| jobs.take(order));
            jobs.wait().await;
            return Ok(0);
        }
        Some(Command::Supervisor) => return supervisor::run(&Paths::discover(), &cwd).await,
        Some(Command::Status) => return machine::status(&Paths::discover()).await,
        Some(Command::Hub { action: HubAction::Up { entry, name } }) => {
            return machine::hub_up(&Paths::discover(), entry.as_deref(), name.as_deref()).await;
        }
        Some(Command::Hub { action: HubAction::Down }) => return machine::down(&Paths::discover(), "hub"),
        Some(Command::Hub { action: HubAction::Here { force, yes } }) => {
            return machine::hub_here(&Paths::discover(), *force, *yes).await;
        }
        Some(Command::Node { action: NodeAction::Up { name, code, entry } }) => {
            let paths = Paths::discover();
            return machine::node_up(&paths, name.as_deref(), code.as_deref(), entry.as_deref()).await;
        }
        Some(Command::Node { action: NodeAction::Run }) => return node::run(&Paths::discover()).await,
        Some(Command::Node { action: NodeAction::Down }) => return machine::down(&Paths::discover(), "node"),
        Some(Command::Node { action: NodeAction::Leave }) => return machine::node_leave(&Paths::discover()).await,
        Some(Command::Relay { action: RelayAction::Up { hub_key, listen } }) => {
            return machine::relay_up(&Paths::discover(), hub_key.as_deref(), listen.as_deref()).await;
        }
        Some(Command::Relay { action: RelayAction::Run }) => {
            let paths = Paths::discover();
            let settings = nypi_core::config::Settings::load(&paths, &paths.home).unwrap_or_default();
            return nypi_hub::relay::run(&paths, settings.relay).await;
        }
        Some(Command::Relay { action: RelayAction::Down }) => return machine::relay_down(&Paths::discover()),
        Some(Command::Update { action: Some(UpdateAction::Keygen { out }), .. }) => {
            let public = update::keygen(out)?;
            println!("The private key is in {} (owner-only; keep it out of every repository).", out.display());
            println!("Build releases with: NYPI_RELEASE_PUBKEY={public}");
            return Ok(0);
        }
        Some(Command::Update { action: Some(UpdateAction::Sign { manifest, key }), .. }) => {
            let signed = update::sign(manifest, key.as_deref())?;
            println!("Wrote {}", signed.display());
            return Ok(0);
        }
        Some(Command::Update { action: Some(UpdateAction::Rollback), .. }) => {
            let paths = Paths::discover();
            let back = update::rollback(&paths)?;
            let back = if back.is_empty() { "the previous version".to_string() } else { format!("v{back}") };
            println!("Rolled back to {back}.");
            supervisor::restart()?;
            return Ok(0);
        }
        Some(Command::Update { node: None, all: false, check, version, yes, allow_unsigned, .. }) => {
            return machine::update_here(&Paths::discover(), *check, version.clone(), *yes, *allow_unsigned).await;
        }
        Some(Command::Send { text, no_wait }) => {
            let Some(link) = client::to_daemon(&Paths::discover(), false).await? else { return Ok(1) };
            return client::send(link, &text.join(" "), !*no_wait).await;
        }
        Some(Command::Attach) => {
            let paths = Paths::discover();
            let Some(link) = client::to_daemon(&paths, true).await? else { return Ok(1) };
            return client::attach(link, &paths, None, !cli.common.hide_thinking).await;
        }
        Some(Command::Trust { action }) => {
            let paths = Paths::discover();
            return match action {
                None => trust::status(&paths, &cwd),
                Some(TrustCommand::Yes) => trust::decide(&paths, &cwd, "yes"),
                Some(TrustCommand::Parent) => trust::decide(&paths, &cwd, "parent"),
                Some(TrustCommand::No) => trust::decide(&paths, &cwd, "no"),
                Some(TrustCommand::Forget) => trust::decide(&paths, &cwd, "forget"),
            };
        }
        _ => {}
    }
    // Resuming runs in the task's own session and directory, whatever this shell's are.
    let resume = match &cli.command {
        Some(Command::Tasks { action: Some(TasksCommand::Resume { id }), .. }) => {
            Some(tasks::resumable(&Paths::discover(), id)?)
        }
        _ => None,
    };
    let cwd = resume.as_ref().map(|task| task.cwd.clone()).unwrap_or(cwd);

    // With the daemon running, this process is an interface of it, not another Lucia (decision
    // D66): a terminal attaches to its folder's thread, and what a schedule or a review has to
    // say is handed to the daemon. Only what needs a runtime of its own stays here.
    let paths = Paths::discover();
    // A person's own message, as opposed to a job a helper process runs or `--json` prints.
    let mut way = match cli.command.is_none() && !cli.json {
        true => {
            Some(Way::of(&cli, &nypi_core::config::Settings::load(&paths, &cwd).unwrap_or_default(), &paths, &cwd)?)
        }
        false => None,
    };
    if cli.common.on.is_some() && (cli.common.standalone || !client::hub_running(&paths)) {
        anyhow::bail!(
            "--on works through the hub, which holds the conversation while the device's node does the work. \
             Start it with nypi hub up (or nypi hub run)."
        );
    }
    if !cli.common.standalone
        && client::hub_running(&paths)
        && let Some(code) = through_the_daemon(&cli, &paths, &cwd, resume.as_ref(), &mut way).await?
    {
        return Ok(code);
    }
    // No daemon here, and this machine is a node of a hub (decision D93): on a personal node the
    // conversation is that hub's, reached over the node's own link; an exec-only node cannot
    // talk to it. A session of this terminal's own is still what `--standalone` gives.
    if !cli.common.standalone
        && !client::hub_running(&paths)
        && way.is_some()
        && needs_own_runtime(&cli, &nypi_core::config::Settings::load(&paths, &cwd).unwrap_or_default()).is_none()
        && let Some(talk) = way.take()
    {
        match client::Link::paired(&paths, Some(&cwd), talk.answers()).await {
            Ok(Some(link)) => return talk.talk(link, &paths, !cli.common.hide_thinking).await,
            Ok(None) => way = Some(talk),
            Err(error) if error.to_string().contains("exec-only") => anyhow::bail!("{error:#}"),
            Err(error) => anyhow::bail!(
                "this machine is a node of a hub, which cannot be reached now: {error:#}\n(nypi --standalone runs a \
                 session here instead)"
            ),
        }
    }

    let common = &cli.common;
    let session = if let Some(task) = &resume {
        SessionMode::Open(task.session_path.clone().expect("resumable tasks have a session file"))
    } else if common.no_session {
        SessionMode::Ephemeral
    } else if let Some(path) = &common.session {
        SessionMode::Open(path.clone())
    } else if matches!(cli.command, Some(Command::Telegram | Command::Hub { action: HubAction::Run })) {
        // The hub's one conversation, whichever folder the daemon was started in (decision D79).
        SessionMode::Primary
    } else if common.continue_session {
        SessionMode::Continue
    } else {
        SessionMode::New
    };
    let thinking = match &common.thinking {
        Some(text) => Some(ThinkingLevel::parse(text).with_context(|| format!("Unknown thinking level: {text}"))?),
        None => None,
    };
    // The daemon's conversation reaches every device whose node connects to this hub, and works
    // where each message's sender is; a terminal's own runtime has the one place it was started in.
    let nodes = matches!(cli.command, Some(Command::Telegram | Command::Hub { action: HubAction::Run }))
        .then(|| nypi_hub::nodes::Nodes::starting(&Paths::discover()));
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd,
        model: common.model.clone(),
        thinking,
        models: common.models.clone(),
        tools: common.tools.clone(),
        session,
        // Every conversation is told what else is being worked on, the daemon's and a
        // terminal's own alike: the ledger is shared, and reading it is cheap.
        active_tasks: true,
        devices: nodes.clone().map(|nodes| nodes as std::sync::Arc<dyn nypi_core::place::DeviceLookup>),
        ..Default::default()
    })?;

    for error in runtime.connect_mcp().await {
        eprintln!("warning: {error}");
    }
    // Startup alerts were printed above; interfaces only need alerts raised from here on.
    if let Some(mut alerts) = runtime.take_alerts() {
        while alerts.try_recv().is_ok() {}
        runtime.restore_alerts(alerts);
    }

    if matches!(cli.command, Some(Command::Telegram)) {
        // Telegram is a channel of the hub; with no daemon running, this becomes the daemon.
        match telegram::api::bot_token(&paths, &runtime.settings) {
            Ok(Some(_)) => {}
            Ok(None) => anyhow::bail!(
                "No bot token. Create a bot with @BotFather, then store its token with `nypi secrets set \
                 channels/telegram` and set channels.telegram.botTokenSecret to cred://channels/telegram (or add \
                 TELEGRAM_BOT_TOKEN=<token> to ~/.env)"
            ),
            Err(why) => anyhow::bail!("Telegram cannot start: {why}"),
        }
        if client::hub_running(&paths) {
            anyhow::bail!(
                "The hub is running without its Telegram channel (channels.telegram.enabled is off, or the bot did not \
                 answer). See {}",
                daemon::log_path(&paths).display()
            );
        }
        println!("Telegram is a channel of the hub. Starting the hub here; Ctrl-C stops it.");
        println!("To pair a chat, run this in another terminal: nypi telegram");
        return daemon::run(runtime, true, nodes.expect("made for the hub")).await;
    }
    if matches!(cli.command, Some(Command::Hub { action: HubAction::Run })) {
        return daemon::run(runtime, false, nodes.expect("made for the hub")).await;
    }
    if let Some(task) = resume {
        return print_mode(&mut runtime, Job::Resume(task.id), cli.json, !common.hide_thinking).await;
    }
    if let Some(Command::Dmn { action: Some(DmnCommand::Run { why }) }) = &cli.command {
        let why = match why.as_str() {
            "idle" => nypi_core::dmn::Why::Idle,
            "interval" => nypi_core::dmn::Why::Interval,
            _ => nypi_core::dmn::Why::Manual,
        };
        return dmn::run(runtime, why, cli.json).await;
    }
    if matches!(cli.command, Some(Command::Goals { action: Some(GoalsCommand::Review) })) {
        runtime.set_channel("goals");
        // A review usually runs from cron or launchd; its result always goes out.
        runtime.settings.notify.after_seconds = 0;
        let text = nypi_core::goals::REVIEW_PROMPT.to_string();
        let job = Job::Sourced { text, source: nypi_core::pulse::PulseSource::module("goals"), cause: None };
        return print_mode(&mut runtime, job, cli.json, !common.hide_thinking).await;
    }
    if matches!(cli.command, Some(Command::Rpc)) {
        rpc::run(runtime).await?;
        return Ok(0);
    }
    let Some(way) = way else {
        // `--json`: every event of this process's own run, as lines.
        let prompt = print_prompt(&cli.prompt.join(" "))?;
        return print_mode(&mut runtime, Job::Prompt(prompt), true, !common.hide_thinking).await;
    };

    // No daemon (or a session of its own was asked for): the hub runs in this process, on the
    // runtime these options made, and the same interface talks to it (decision D67, part 2).
    // A chat model asked for on the command line must exist; one in the settings that does not
    // is warned about and left out, as the daemon does, rather than stopping the terminal.
    if let Some(model) = &common.chat_model
        && runtime.registry.find(model).is_none()
    {
        anyhow::bail!("The chat model {model} is not a known model (nypi models lists them)");
    }
    let chat = common.chat_model.clone().or_else(|| runtime.settings.chat.model.clone()).filter(|_| !common.no_chat);
    let chat = chat.filter(|model| {
        let known = runtime.registry.find(model).is_some();
        if !known {
            eprintln!("warning: chat.model {model} is not a known model; this conversation runs without a chat module");
        }
        known
    });
    runtime.settings.chat.model = chat;
    let link = client::Link::local(runtime, way.answers()).await?;
    way.talk(link, &paths, !common.hide_thinking).await
}

/// How a person's message is taken and answered: one printed answer, the full-screen UI, or the
/// line-based terminal. The same three over a link to the daemon and over one to a hub of this
/// process.
enum Way {
    Print(String),
    FullScreen(Option<String>),
    Terminal(Option<String>),
}

impl Way {
    /// Decide from the invocation. A person at a terminal is asked once whether this folder's
    /// own configuration may load; every other way of starting leaves it out until they decide
    /// (decision D61).
    fn of(
        cli: &Cli,
        settings: &nypi_core::config::Settings,
        paths: &Paths,
        cwd: &std::path::Path,
    ) -> anyhow::Result<Way> {
        let prompt = cli.prompt.join(" ");
        if cli.print {
            return Ok(Way::Print(print_prompt(&prompt)?));
        }
        let at_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if at_terminal && !unattended() {
            trust::ask(paths, cwd)?;
        }
        let first = (!prompt.is_empty()).then_some(prompt);
        let full_screen = (cli.common.tui || settings.tui) && !cli.common.no_tui && at_terminal;
        Ok(if full_screen { Way::FullScreen(first) } else { Way::Terminal(first) })
    }

    /// Whether somebody there can answer an approval.
    fn answers(&self) -> bool {
        !matches!(self, Way::Print(_))
    }

    async fn talk(self, link: client::Link, paths: &Paths, show_thinking: bool) -> anyhow::Result<i32> {
        match self {
            Way::Print(prompt) => client::send(link, &prompt, true).await,
            Way::FullScreen(first) => tui::run(link, paths, first, show_thinking).await,
            Way::Terminal(first) => client::attach(link, paths, first, show_thinking).await,
        }
    }
}

/// Why this invocation needs a runtime of its own even though the daemon runs, if it does.
fn needs_own_runtime(cli: &Cli, settings: &nypi_core::config::Settings) -> Option<&'static str> {
    let common = &cli.common;
    if cli.json || cli.mode.is_some() {
        Some("--json and --mode print this process's own events")
    } else if common.model.is_some() || common.thinking.is_some() || common.models.is_some() || common.tools.is_some() {
        Some("a model, thinking level, or tool set of its own was asked for")
    } else if common.session.is_some() || common.no_session {
        Some("a particular session was asked for")
    } else if common.chat_model.is_some() || (settings.chat.model.is_some() && common.no_chat) {
        Some("a chat model of its own, or none, was asked for")
    } else {
        None
    }
}

/// Do this invocation's work through the running daemon. None when it has to run here.
async fn through_the_daemon(
    cli: &Cli,
    paths: &Paths,
    cwd: &std::path::Path,
    resume: Option<&nypi_core::ledger::TaskRecord>,
    way: &mut Option<Way>,
) -> anyhow::Result<Option<i32>> {
    match &cli.command {
        Some(Command::Goals { action: Some(GoalsCommand::Review) }) => {
            let source = serde_json::to_value(nypi_core::pulse::PulseSource::module("goals"))?;
            let taken = client::deliver(paths, None, nypi_core::goals::REVIEW_PROMPT, &source, "goals").await?;
            if taken {
                println!("The goal review was handed to the hub; its result reaches you as a notice.");
            }
            Ok(taken.then_some(0))
        }
        // A thread that opens resumes what its conversation's last process left. When the task
        // belongs to another conversation in that folder, it is resumed here as before.
        Some(Command::Tasks { action: Some(TasksCommand::Resume { .. }), .. }) => {
            let Some(task) = resume else { return Ok(None) };
            let session = client::thread_session(paths, &task.cwd, task.remote.as_ref()).await?;
            if let Some(remote) = &task.remote
                && session.as_deref() != Some(task.session_id.as_str())
            {
                // Only the daemon reaches a device; this computer's tools must not take it up.
                anyhow::bail!("task {} works at {remote}; the hub resumes it while that device is connected", task.id);
            }
            if session.as_deref() != Some(task.session_id.as_str()) {
                return Ok(None);
            }
            println!("Task {} belongs to the hub's conversation in {}; it resumes it.", task.id, task.cwd.display());
            Ok(Some(0))
        }
        None => {
            let settings = nypi_core::config::Settings::load(paths, cwd).unwrap_or_default();
            if let Some(why) = needs_own_runtime(cli, &settings) {
                if cli.common.on.is_some() {
                    anyhow::bail!("--on works through the hub, but {why}");
                }
                if std::io::stderr().is_terminal() {
                    eprintln!("[the hub is running, but {why}: this is a separate session]");
                }
                return Ok(None);
            }
            let Some(talk) = way.take() else { return Ok(None) };
            // A conversation at another device: the daemon holds it, and that device's node does the work.
            if let Some(device) = &cli.common.on {
                let folder = cli.common.at.clone().unwrap_or_else(|| cwd.display().to_string());
                let Some(link) = client::Link::at_device(paths, device, &folder, talk.answers()).await? else {
                    anyhow::bail!("the hub stopped; --on needs it");
                };
                return talk.talk(link, paths, !cli.common.hide_thinking).await.map(Some);
            }
            // The daemon may have stopped since it was last seen: then this runs here after all.
            let Some(link) = client::Link::hub(paths, Some(cwd), talk.answers()).await? else {
                *way = Some(talk);
                return Ok(None);
            };
            talk.talk(link, paths, !cli.common.hide_thinking).await.map(Some)
        }
        _ => Ok(None),
    }
}

/// The prompt of a print run: the arguments, with piped stdin appended.
fn print_prompt(arguments: &str) -> anyhow::Result<String> {
    let mut prompt = arguments.to_string();
    if !std::io::stdin().is_terminal()
        && let Some(input) = read_piped_stdin()
        && !input.trim().is_empty()
    {
        prompt = if prompt.is_empty() { input } else { format!("{prompt}\n\n{input}") };
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("--print needs a prompt");
    }
    Ok(prompt)
}

/// Read piped stdin, unless nothing arrives quickly: callers such as schedulers and agent
/// harnesses often leave stdin as an open pipe that never closes, which must not hang the run.
fn read_piped_stdin() -> Option<String> {
    // First message: whether any data arrived; second: everything read until EOF.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut first = [0u8; 8192];
        let n = stdin.read(&mut first).unwrap_or(0);
        let _ = tx.send(first[..n].to_vec());
        if n > 0 {
            let mut rest = Vec::new();
            let _ = stdin.read_to_end(&mut rest);
            let _ = tx.send(rest);
        }
    });
    let mut bytes = rx.recv_timeout(std::time::Duration::from_millis(300)).ok()?;
    if bytes.is_empty() {
        return None;
    }
    // Data is flowing, so wait for the producer to finish.
    bytes.extend(rx.recv().unwrap_or_default());
    Some(String::from_utf8_lossy(&bytes).into_owned())
}
