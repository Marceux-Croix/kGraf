//! `kgraf-wintest`: see the library's module documentation.

use kgraf_wintest::cli::{self, Command};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match cli::parse(&args) {
        Err(e) => {
            eprintln!("{e}");
            2
        }
        Ok(Command::Help) => {
            print!("{}", cli::USAGE);
            0
        }
        Ok(Command::Run(o)) => kgraf_wintest::runner::run(o),
        Ok(Command::SandboxRun { program, args }) => {
            kgraf_wintest::sandbox_run::run(&program, &args)
        }
        Ok(Command::Pack(o)) => match kgraf_wintest::pack::pack(&o) {
            Ok(summary) => {
                println!("{summary}");
                0
            }
            Err(e) => {
                eprintln!("pack: {e}");
                1
            }
        },
        Ok(Command::E2e { kgraf }) => e2e_only(kgraf),
    };
    std::process::exit(code);
}

/// The end-to-end checks alone, against a given kgraf, in a work folder
/// that is removed afterward. For developing the checks on any host.
fn e2e_only(kgraf: std::path::PathBuf) -> i32 {
    use kgraf_wintest::{e2e, workdir};
    let base = std::env::temp_dir();
    let layout = workdir::layout(&base, &workdir::run_id("e2e", std::process::id()));
    let made = vec![workdir::Made::Folder {
        path: layout.root.display().to_string(),
    }];
    for d in [&layout.e2e, &layout.tmp, &layout.kgraf_home] {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("{e}");
            return 2;
        }
    }
    let env = vec![
        (
            "KGRAF_DIAG_PATH".to_owned(),
            layout.kgraf_home.join("diag.jsonl").display().to_string(),
        ),
        (
            "KGRAF_SETTINGS_PATH".to_owned(),
            layout
                .kgraf_home
                .join("settings.toml")
                .display()
                .to_string(),
        ),
    ];
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let ctx = e2e::Ctx {
        kgraf,
        dir: layout.e2e.clone(),
        env,
        git_bash_found: true,
        // The kgraf under test runs on this machine with the backend this
        // process would pick, so this process's answer is its answer.
        network_open: kgraf_sandbox::CheckNetwork::current()
            .shell_notice()
            .is_some(),
        cancel: &cancel,
    };
    let checks = e2e::run_all(&ctx, cfg!(windows));
    let mut failed = 0;
    for c in &checks {
        println!("{:?} {} {}: {}", c.status, c.id, c.title, c.detail);
        if c.status == e2e::Status::Fail {
            failed += 1;
            println!("{}", c.evidence);
        }
    }
    let cl = workdir::clean_up(&made, &base, &kgraf_wintest::platform::ops());
    println!("cleanup verified: {}", cl.verified);
    i32::from(failed > 0)
}
