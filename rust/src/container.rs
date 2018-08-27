use failure::{Error, ResultExt};
use ipc_channel::ipc::{self, IpcOneShotServer, IpcReceiver, IpcSender};
use nix::errno::Errno;
use nix::sched::CloneFlags;
use nix::sys::wait::{waitpid, WaitStatus::*};
use nix::unistd;
use nix::Error::Sys;
use std::boxed::Box;
use std::fs::OpenOptions;
use std::io::Write;
use std::{thread, time};
use yansi::Paint;

use colors::*;

#[derive(Serialize, Deserialize, Debug)]
struct Ready;

pub fn with_unshared_user_and_mount<F>(mut child_fn: F) -> Result<(), Error>
where
    F: FnMut() -> Result<(), Error>,
{
    // new unshared mount namespace and a new unshared user namespace.
    let clone_flags = CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUSER;

    // TODO(cleanup): Seems to me like from the glibc docs of clone, a stack for
    // the child should only be necessary if CLONE_VM is set.
    const STACK_SIZE: usize = 1024 * 1024;
    let ref mut child_stack: [u8; STACK_SIZE] = [0; STACK_SIZE];

    let (parent_server, parent_name) = init_ipc()?;
    let child_pid =
        ::nix::sched::clone(
            Box::new(|| {
                // Wait for ready message that UID mapping has been setup before
                // running child_fn. Otherwise, mounting will fail. Also, if the
                // child process attempts to exec before the UID mapping has been
                // setup, then the child will lose its capabilities (see
                // "capabilities(7)" man page).
                match recv_ready(&parent_name).and(child_fn()) {
                    // Exited successfully.
                    Ok(()) => 0,
                    Err(err) => {
                        println!("");
                        println!("{} {}", color_err(&"mzr child error:"), err);
                        1
                    }
                }
            }),
            child_stack,
            clone_flags,
            None,
        ).context("Error while cloning mzr child with unshared user and mount namespaces.")?;

    // Map the current user to root within the child process.
    map_user_to_root(child_pid)?;

    send_ready(parent_server)?;

    // FIXME: Why is this necessary??  Should do something more reliable.
    thread::sleep(time::Duration::from_millis(100));

    match waitpid(child_pid, None) {
        Err(e @ Sys(Errno::ECHILD)) => Err(e).context("Failed to find mzr child after fork.")?,
        Err(e @ Sys(Errno::EINTR)) => {
            Err(e).context("Waiting for mzr child interrupted by signal.")?
        }
        Err(e @ Sys(Errno::EINVAL)) => Err(e).context("Impossible: waitpid was called wrong.")?,
        Err(e) => Err(e).context("Unexpected error in waitpid.")?,
        Ok(Exited(_, status)) => {
            if status == 0 {
                println!("mzr child exited with success.");
            } else {
                println!(
                    "mzr child exited with {} {}",
                    color_err(&"error code"),
                    color_err(&status)
                );
            }
        }
        Ok(Signaled(_, signal, _)) => {
            println!(
                "mzr child was {} {:?}",
                color_err(&"killed by signal"),
                color_err(&signal)
            );
        }
        Ok(status) => {
            // The other status results only occur when particular options are
            // passed to waitpid.
            bail!(
                "Response from waiting for child should be impossible: {:?}",
                Paint::blue(status)
            );
        }
    }

    Ok(())
}

// IPC helper functions

fn init_ipc() -> Result<(IpcOneShotServer<IpcSender<Ready>>, String), Error> {
    wrap_ipc(IpcOneShotServer::new().map_err(|x| x.into()))
}

// TODO(cleanup): Made up this idiom of using an argumentless closure to still
// use the "?" error plumbing, while having a helper that modifies the error
// contents.  Is there a cleaner way to do something like this?

fn send_ready(parent_server: IpcOneShotServer<IpcSender<Ready>>) -> Result<(), Error> {
    wrap_ipc((|| {
        let (_, tx1): (_, IpcSender<Ready>) = parent_server.accept()?;
        tx1.send(Ready)?;
        Ok(())
    })())
}

fn recv_ready(parent_name: &String) -> Result<(), Error> {
    wrap_ipc((|| {
        // Establish a connection with the parent.
        let (tx1, rx1): (IpcSender<Ready>, IpcReceiver<Ready>) = ipc::channel()?;
        let tx0 = IpcSender::connect(parent_name.to_string())?;
        tx0.send(tx1)?;
        let Ready = rx1.recv()?;
        Ok(())
    })())
}

fn wrap_ipc<T>(x: Result<T, Error>) -> Result<T, Error> {
    Ok(x.context("Error encountered in interprocess communication mechanism.")?)
}

// UID mapping helper functions
fn map_user_to_root(child_pid: unistd::Pid) -> Result<(), Error> {
    wrap_user_mapping((|| {
        // Map current user to root within the user namespace.
        let uid_map_path = format!("/proc/{}/uid_map", child_pid);
        let mut uid_map_file = OpenOptions::new().write(true).open(uid_map_path)?;
        uid_map_file.write_all(format!("0 {} 1\n", unistd::Uid::current()).as_bytes())?;

        // Disable usage of setgroups system call, allowing gid_map to
        // be written.
        let set_groups_path = format!("/proc/{}/setgroups", child_pid);
        let mut set_groups_file = OpenOptions::new().write(true).open(set_groups_path)?;
        set_groups_file.write_all(b"deny")?;

        // Map current group to root within the user namespace.
        let gid_map_path = format!("/proc/{}/gid_map", child_pid);
        let mut gid_map_file = OpenOptions::new().write(true).open(gid_map_path)?;
        gid_map_file.write_all(format!("0 {} 1\n", unistd::Gid::current()).as_bytes())?;
        Ok(())
    })())
}

// TODO(cleanup)
fn wrap_user_mapping<T>(x: Result<T, Error>) -> Result<T, Error> {
    Ok(x.context(
        "Error encountered while mapping user to root within the child process user namespace.",
    )?)
}
