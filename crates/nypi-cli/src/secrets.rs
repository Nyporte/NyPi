//! `nypi secrets`: the owner's side of the secret store (decision D77). `set` takes a secret's
//! value from the terminal without showing it (or from a pipe) and seals it under a handle;
//! `list` prints handles and when each was set, never a value; `rm` takes one away. The agent
//! has no such command: it names a handle, and the harness uses the secret for it.

use std::io::{IsTerminal, Read, Write};

use nypi_core::config::Paths;
use nypi_vault::secrets::{Handle, SecretStore};

/// Read one line from the terminal with echo off, restoring the terminal whatever happens.
fn read_hidden(prompt: &str) -> anyhow::Result<String> {
    use std::os::fd::AsRawFd;
    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut before = std::mem::MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: `before` is a zeroed termios that tcgetattr fills; it is read only on success.
    if unsafe { libc::tcgetattr(fd, before.as_mut_ptr()) } != 0 {
        anyhow::bail!("the terminal's settings could not be read");
    }
    // SAFETY: filled by the successful call above.
    let before = unsafe { before.assume_init() };
    let mut hidden = before;
    hidden.c_lflag &= !libc::ECHO;
    // SAFETY: `hidden` is a valid termios copied from the terminal's own.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) };
    let mut line = String::new();
    let read = stdin.read_line(&mut line);
    // SAFETY: restores the settings read above.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &before) };
    eprintln!();
    read?;
    Ok(line)
}

/// The value for `set`: typed unseen at a terminal, or everything on a pipe. One trailing
/// newline is not part of it.
pub fn read_value(handle: &Handle) -> anyhow::Result<String> {
    let mut value = if std::io::stdin().is_terminal() {
        read_hidden(&format!("Value for {handle} (not shown): "))?
    } else {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped)?;
        piped
    };
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    Ok(value)
}

pub fn set(paths: &Paths, name: &str) -> anyhow::Result<i32> {
    let handle = Handle::parse(name).map_err(anyhow::Error::msg)?;
    let value = read_value(&handle)?;
    if value.is_empty() {
        anyhow::bail!("nothing was given; {handle} is unchanged");
    }
    SecretStore::open(&paths.home)?.set(&handle, &value)?;
    nypi_hub::outln!("Stored {handle}. Lucia can use it by that name; she never sees its value.");
    Ok(0)
}

/// `secrets set` with the value read on another of the user's machines and sent over its link
/// (decision D96). The value is in no output, log, or audit record.
pub fn set_value(paths: &Paths, name: &str, value: &str) -> anyhow::Result<i32> {
    let handle = Handle::parse(name).map_err(anyhow::Error::msg)?;
    if value.is_empty() {
        anyhow::bail!("nothing was given; {handle} is unchanged");
    }
    SecretStore::open(&paths.home)?.set(&handle, value)?;
    nypi_hub::outln!("Stored {handle} on the hub. Lucia can use it by that name; she never sees its value.");
    Ok(0)
}

pub fn list(paths: &Paths) -> anyhow::Result<i32> {
    // Without a store there is nothing to list, and no reason to make a master key.
    if !paths.secrets().exists() {
        nypi_hub::outln!("No secrets are stored. Add one with: nypi secrets set <scope>/<name>");
        return Ok(0);
    }
    let listed = SecretStore::open(&paths.home)?.list()?;
    if listed.is_empty() {
        nypi_hub::outln!("No secrets are stored. Add one with: nypi secrets set <scope>/<name>");
    }
    for (handle, updated) in listed {
        nypi_hub::outln!("{handle}   set {updated}");
    }
    Ok(0)
}

pub fn remove(paths: &Paths, name: &str) -> anyhow::Result<i32> {
    let handle = Handle::parse(name).map_err(anyhow::Error::msg)?;
    if !paths.secrets().exists() || !SecretStore::open(&paths.home)?.remove(&handle)? {
        nypi_hub::outln!("There is no {handle}.");
        return Ok(1);
    }
    nypi_hub::outln!("Removed {handle}.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_and_removing_need_no_store_and_show_no_value() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        assert_eq!(list(&paths).unwrap(), 0);
        assert!(!paths.secrets_key().exists(), "looking at an empty store makes no key");
        assert_eq!(remove(&paths, "github/bot").unwrap(), 1);
        let handle = Handle::parse("github/bot").unwrap();
        SecretStore::open(&paths.home).unwrap().set(&handle, "tok-123456").unwrap();
        assert_eq!(list(&paths).unwrap(), 0);
        assert_eq!(remove(&paths, "cred://github/bot").unwrap(), 0);
        assert!(set(&paths, "not a handle").is_err());
    }
}
