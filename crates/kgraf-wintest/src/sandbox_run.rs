//! `kgraf-wintest sandbox-run -- <program> [args...]`: run one program
//! inside kGraf's real sandbox, for manual tests.
//!
//! Only the public kgraf-sandbox API: a policy with the network allowed and
//! partial isolation accepted, its one writable folder (and `TEMP`) inside a
//! fresh work folder, which is removed and checked afterward. Output goes
//! straight to this console.

use kgraf_sandbox::{Network, Redirect, SandboxPolicy, sandboxed_command};

use crate::workdir::{self, Made};

pub fn run(program: &str, args: &[String]) -> i32 {
    let base = std::env::temp_dir();
    let id = workdir::run_id("sandbox-run", std::process::id());
    let layout = workdir::layout(&base, &id);
    let folder = layout.e2e.join("box");
    let made = vec![Made::Folder {
        path: layout.root.display().to_string(),
    }];
    let code = match std::fs::create_dir_all(&folder)
        .and_then(|()| std::fs::create_dir_all(&layout.tmp))
    {
        Err(e) => {
            eprintln!(
                "cannot create the work folder {}: {e}",
                layout.root.display()
            );
            2
        }
        Ok(()) => run_in(program, args, &folder, &layout.tmp),
    };
    let c = workdir::clean_up(&made, &base, &crate::platform::ops());
    if c.verified {
        eprintln!(
            "[kgraf-wintest] work folder removed: {}",
            layout.root.display()
        );
    } else {
        eprintln!(
            "[kgraf-wintest] work folder NOT fully removed: {:?} {:?}",
            c.left, c.notes
        );
    }
    code
}

fn run_in(program: &str, args: &[String], folder: &std::path::Path, tmp: &std::path::Path) -> i32 {
    let policy = SandboxPolicy::new(
        vec![folder.to_path_buf(), tmp.to_path_buf()],
        Network::Allow,
    )
    .accept_partial_isolation(true)
    .with_tmp_dir(tmp.to_path_buf());
    let mut cmd = match sandboxed_command(&policy, program, folder) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[kgraf-wintest] the sandbox refused the command: {e}");
            return 2;
        }
    };
    cmd.args(args)
        .stdin(Redirect::Null)
        .stdout(Redirect::Inherit)
        .stderr(Redirect::Inherit);
    eprintln!(
        "[kgraf-wintest] running in kGraf's sandbox (network allowed), folder {}",
        folder.display()
    );
    match cmd.status() {
        Ok(s) => {
            let code = s.code().unwrap_or(-1);
            eprintln!("[kgraf-wintest] exit code: {code}");
            code
        }
        Err(e) => {
            eprintln!("[kgraf-wintest] the program did not run: {e}");
            2
        }
    }
}
