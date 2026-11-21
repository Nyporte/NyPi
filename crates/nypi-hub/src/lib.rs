//! The hub (decisions D66, D67, and D79): the one place Lucia runs. It holds her conversation
//! (`daemon`), speaking the protocol interfaces use (`rpc`, and `chat_loop` where the chat
//! module is in front), carries out the work nobody asked for in a conversation
//! (`jobs`, ordered by the `scheduler` and the `watchdog`), runs her message channels
//! (`channels`, Telegram among them), and tells the user what nobody was there to see (`notify`). `link` is
//! the other end: how an interface, or another process, reaches the conversation. `nodes` is the
//! register of the devices whose nodes carry out her calls; `instance` says what this machine is,
//! `pairing` is how a machine joins the hub and stays what it was admitted as, and `reach`,
//! `direct`, `mdns`, `ws`, and `relay` are how machines and browsers reach the hub, directly or
//! through the relay behind the entry (decision D94); `state` and `standby` are the hub's state
//! as it is copied to personal machines and moves to one of them (decision D95).
//!
//! This crate holds no command line and draws nothing: `nypi-cli` is the program, its terminal
//! and full-screen interfaces, the supervisor, and the board.

pub mod bridge;
pub mod channels;
pub mod chat_loop;
pub mod commands;
pub mod daemon;
pub mod direct;
pub mod idle_pass;
pub mod instance;
pub mod jobs;
pub mod link;
pub mod mdns;
pub mod nodes;
pub mod notify;
pub mod output;
pub mod pairing;
pub mod protocol;
pub mod rating;
pub mod reach;
pub mod records;
pub mod relay;
pub mod round_lock;
pub mod rpc;
pub mod scheduler;
pub mod standby;
pub mod state;
pub mod watchdog;
pub mod ws;
