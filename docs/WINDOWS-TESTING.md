# Testing kGraf on Windows

The Windows sandbox (decision 0011) and the worker's shell on Windows are built and
compile-checked from macOS, but nothing in them has run on a Windows machine yet. These
steps test them on a fresh Windows 11 machine, on x86_64 (Intel or AMD) and on ARM64
(Snapdragon). They take about an hour, most of it the first build.

Run every command in **Windows PowerShell or PowerShell 7, as a normal user** (not "Run as
administrator"): the sandbox is made for a user without administrator rights, and an
elevated shell would test something else.

## 0. The self-test bundle (the first choice)

Most of the steps below are automated in one program, `kgraf-wintest.exe`, shipped in a zip
with kGraf's test binaries cross-built for Windows. It needs Git for Windows and nothing
else: no Rust, no build tools, no API key, no administrator rights. It installs nothing.

1. Build the zip on macOS (or Linux) with `scripts/package-wintest.sh`. It writes
   `dist/wintest/kgraf-wintest-<version>-<commit>-windows-x86_64.zip`.
2. On the Windows machine: unblock the zip (Properties, "Unblock"), unzip it, double-click
   `kgraf-wintest.exe`, answer whether to run the extended set (or wait 20 seconds for yes).
3. Send back the one report it writes to the Desktop,
   `kgraf-wintest-report-<date>-<time>.md`. Delete the unzipped folder.

What it runs, in this order: the machine facts (Windows build and edition, architecture and
emulation, processor, memory, Git for Windows and Git Bash, winget, Defender, SmartScreen
and Smart App Control, locale); the core set (every kgraf-sandbox test binary, the
`windows` tests of kgraf-tools, the network notice, dependency link and check input tests
of kgraf-host, the orchestrator's network notice test); end-to-end checks with the real
`kgraf.exe` (sections 5, 6 and 6a below, with a scripted model on 127.0.0.1 in place of a
provider); then, if chosen, the extended set (every other test binary of the workspace that
runs without Rust, a model, Docker or the internet). Each test binary runs with
`--test-threads=1`. The report has a summary table first, then every failure with its
output, the per-binary counts and times, what was not run and why, and the same data as
JSON at the end. The report replaces the user name and computer name, and records no
environment variables.

libtest prints a failed test's panic and captured output only at the end of a binary's run,
so a binary that hangs (killed at its time limit) or crashes loses it. The runner then runs
each test the binary reported FAILED, and the test that was running, alone again
(`<binary> --exact <name> --test-threads=1`, same environment and working folder). Each
re-run has a 120 second limit (`SOLO_RERUN_SECS`); a binary gets at most 30 re-runs
(`MAX_SOLO_RERUNS`) and 10 minutes of re-runs in all (`SOLO_RERUN_BUDGET_SECS`), the running
test first. The report uses that output as the test's tail, says it came from a solo re-run,
and says whether the re-run passed, failed or hung (JSON: `solo` on each failure,
`open_test_solo`). The test's outcome from the first run is not changed. Tests over the cap
or the time budget are listed under "not re-run alone" (JSON: `rerun_skipped`).

How it runs tests built on another machine: a test binary holds its package folder as the
build machine spelled it (`env!("CARGO_MANIFEST_DIR")`) and reads fixtures and starts
programs (`CARGO_BIN_EXE_*`) under it. The runner copies the sources and programs into its
work folder at that same relative path, maps a free drive letter there with `subst`, and
runs each binary on that drive, so those paths resolve with no change to the tests. Tests
that build a helper with cargo at run time, need a model key or Docker, or read the
repository's git history are left out; the manifest in the zip lists each with its reason.

What it leaves behind: the report only. Every file the run makes (the sources, the test
processes' `TEMP`, kGraf's diagnostics and settings files, the end-to-end repositories, the
folders the sandbox labels) is in one work folder under `%TEMP%`. At the end, also after
Ctrl+C, it removes the drive letter and the work folder (resetting permissions and retrying
once if needed; links are removed, never followed), then checks that both are gone, that no
`kgraf-escape-*` file or Run key value from an escape test remains, and that no kGraf folder
appeared under `%APPDATA%` or `%LOCALAPPDATA%`; the report lists what was created and
removed. It offers to delete the unzipped folder after it exits.

What stays manual: installing the prerequisites and building from source (sections 2 and
3), the winget install itself and its question in the app and on a terminal (section 6a
cases 1 to 3; the runner simulates a missing Git Bash with `KGRAF_GIT_BASH_PATH` naming a
missing file and with `winget` taken off `PATH`, and marks those checks SIMULATED), the
different Git install locations (case 4), the sandboxed dependency install (section 7, which
needs npm and the internet), the desktop app window and installer, and the
Windows Firewall test in `MANUAL-NETWORK-TEST.txt` in the zip. For manual tests,
`kgraf-wintest.exe sandbox-run -- <program> [args...]` runs one program inside kGraf's
sandbox in a temporary folder and prints its exit code.

Zip layout:

```
kgraf-wintest-<version>-<commit>-windows-x86_64/
  kgraf-wintest.exe        the runner
  README.txt               the steps above
  MANUAL-NETWORK-TEST.txt  the Windows Firewall test (administrator)
  bundle-manifest.json     what runs, in which set, and what is left out and why
  tests\                   the test binaries
  programs\                kgraf.exe and the programs tests start by path
  src\                     the sources and fixtures the tests read
```

The bundle is built for x86_64 only; on ARM64 it runs under emulation, and the report says
so. The steps below remain the fallback, and the way to test a native ARM64 build.

### Running the bundle in GitHub Actions

The `wintest.yml` workflow builds the bundle natively on a GitHub-hosted Windows runner
(`KGRAF_WINTEST_NATIVE=1 scripts/package-wintest.sh`), runs `kgraf-wintest.exe --all
--no-wait` (with `--sandbox-user` by default, see below), and runs the Windows Firewall test (`scripts/wintest-firewall.ps1`). It has a
manual trigger only:

```
gh workflow run wintest.yml
gh run watch
gh run download <run id> -n kgraf-windows-selftest
```

The artifact holds `report\kgraf-wintest-report-*.md` and `firewall.txt`. It bills Windows
minutes (twice the Linux rate on a private repository). A failing test does not fail the
job; a missing report file does. Two things differ from a standard user's machine: the
runner account is an administrator, so the sandbox and firewall checks run elevated, and
the runner has Git for Windows, so the worker shell finds `sh`. A result from the runner
does not replace a run on a standard-user machine without Git for Windows.

#### The sandbox user phase

The workflow has one input, `sandbox_user`, on by default. It adds `--sandbox-user` to the
runner. Turn it off with `gh workflow run wintest.yml -f sandbox_user=false`. A person can
add the same flag on any Windows machine: `kgraf-wintest.exe --all --sandbox-user`.

The suite runs first, exactly as without the flag, with no sandbox user set up. That is the
baseline, and it stays comparable with earlier runs. Then the phase runs these steps:

| Step | Command | What it checks |
|---|---|---|
| (b0) | `kgraf sandbox selfcheck` | Before setup, for comparison only. On the token sandbox it passes with a network line that is not a pass. |
| (b) | `kgraf sandbox setup` | Creates the sandbox users (two per worker slot, one without the network and one with it, plus the two fixed users), the group and the firewall rules, with no tag. The output lists a received password for every one of these users. |
| (c) | `kgraf sandbox status` | Every kGraf line is present, for both users of every slot. Every stored password logs on. |
| (d) | `kgraf sandbox selfcheck` | The gate. It passes only if selfcheck exits 0, prints `sandbox selfcheck: ok` and names the backend `Windows sandbox user`. A pass on the token sandbox means setup did not take effect, so it stops the phase too. If the gate fails, step (e) does not run. |
| (e) | re-run | The sandbox test binaries, the baseline binaries that failed for the cargo jobserver, Git Bash or Schannel, and the end-to-end checks, with the sandbox user active. |
| (f) | `kgraf sandbox remove`, then `kgraf sandbox status` | Always runs, also after a failure or Ctrl+C. Every kGraf line must be MISSING. |

Rules of the phase:

- It uses the sandbox user of the Windows user who runs it (no tag). If that user already has
  a stored sandbox user, the phase does not start, because step (f) would remove it. Run
  `kgraf sandbox remove` first.
- Setup needs administrator rights. The hosted runner is elevated. On another machine, start
  the runner from an elevated prompt, or accept the UAC prompt. The report says so when the
  window is not elevated and a step fails.
- `kgraf-sandbox-runner.exe` must be in `programs\` of the bundle. The phase puts a copy in
  `tests\` for the test binaries, and removes it at the end.
- `KGRAF_SANDBOX_SETUP_TEST=1` stays only for the tagged setup test in the baseline. The
  phase does not use a tag.
- The status command prints no words "logon works". A stored password line that is
  `present` means that a real logon with that password worked, and the report counts them.

What the report part "Sandbox user phase" shows:

- A summary line in the first table: it ran to the end, or it stopped at the gate, and
  whether the sandbox user is gone.
- The commands of (b), (c), (d) and (f) with their full output. If the gate fails, the full
  selfcheck output is there and the report says that the phase stopped at the gate.
- A table with the baseline and the sandbox user side by side for each re-run binary. A
  second table lists each test that failed in the baseline and ran again: `passed`,
  `failed`, `hung` or `not reached`. The tail of each is below the table.
- The end-to-end checks with the baseline status and the sandbox user status.
- The confirmation after (f). If anything is left, the runner exits with a non-zero code.

The totals and the verdict at the top of the report count the baseline only.

## 1. Find out which machine you have

```powershell
$env:PROCESSOR_ARCHITECTURE
```

`AMD64` is an x86_64 machine. `ARM64` is an ARM64 machine. Steps that differ say so.

## 2. Install the prerequisites

Each `winget` command installs one program. Accept the prompts. Close and reopen
PowerShell after the last one, so that it sees the new `PATH`.

**Git for Windows** (it also provides Git Bash, which kGraf uses to run `sh` and `bash`
commands). `winget` picks the native build for your machine, ARM64 included:

```powershell
winget install --id Git.Git -e --source winget
```

**The Microsoft C++ build tools and the Windows SDK** (the Rust MSVC toolchain links with
them). This is the command the Rust installer's own documentation gives; it adds the x64 and
the ARM64 compilers on either machine:

```powershell
winget install --id Microsoft.VisualStudio.2022.BuildTools --source winget --force --override "--wait --passive --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --add Microsoft.VisualStudio.Component.VC.Llvm.Clang --add Microsoft.VisualStudio.Component.VC.Llvm.ClangToolset --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --addProductLang En-us"
```

The two `Llvm.Clang` components are for ARM64: one of kGraf's dependencies (`aws-lc-sys`,
the TLS library) compiles its C code with `clang-cl` there. If `winget` reports that a newer
Visual Studio replaced 2022, run `winget search Microsoft.VisualStudio` and use the
`BuildTools` id it lists with the same `--override` text.

**NASM** (x86_64 only: `aws-lc-sys` assembles its x86_64 code with it):

```powershell
winget install --id NASM.NASM -e --source winget
```

If NASM cannot be installed, set `$env:AWS_LC_SYS_PREBUILT_NASM = "1"` in the PowerShell
window before you build instead; `aws-lc-sys` then uses its checked-in prebuilt objects.

**Rust**, through rustup:

```powershell
winget install --id Rustlang.Rustup -e --source winget
```

Reopen PowerShell, then check each tool:

```powershell
rustup show
cargo --version
git --version
& "$env:ProgramFiles\Git\bin\bash.exe" -c "uname -a"
```

`rustup show` names the default toolchain: `stable-x86_64-pc-windows-msvc` on x86_64,
`stable-aarch64-pc-windows-msvc` on ARM64. kGraf needs Rust 1.95 or later; if `cargo
--version` is older, run `rustup update`. The last line prints a line starting with
`MINGW64_NT` (x86_64) or `CLANGARM64_NT` or `MINGW64_NT` (ARM64): that is Git Bash.

## 3. Get the code and build it

```powershell
mkdir C:\src -Force
cd C:\src
git clone https://github.com/Marceux-Croix/kGraf.git
cd kGraf
cargo build -p kgraf-cli
```

Keep the project in a folder on an NTFS drive (`C:` is): the sandbox works through file
permissions and refuses a folder on a FAT or exFAT drive. The first build takes 10 to 30
minutes.

**ARM64 only: also build for x86_64.** ARM64 Windows runs x86_64 programs through
emulation, and many users run x86_64 tools there. The native target is
`aarch64-pc-windows-msvc`, which the steps below use by default. To test the emulated
x86_64 build as well, add its target once and repeat every `cargo test` command in step 4
with `--target x86_64-pc-windows-msvc` after `cargo test`:

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build -p kgraf-cli --target x86_64-pc-windows-msvc
```

## 4. Run the tests

Make a folder for the results first; every command below writes its output there too:

```powershell
mkdir C:\src\kgraf-results -Force
$r = "C:\src\kgraf-results"
```

Run each command and let it finish, even when an earlier one failed. None of these tests
is marked `#[ignore]`: the Windows-only test files compile only on Windows, so a plain
`cargo test` runs them there. `--test-threads=1` is needed because some tests change the
process environment.

1. The sandbox's unit tests (the Git Bash search, the argument transport, the network
   decision, and which token property makes Schannel refuse a TLS credential):

   ```powershell
   cargo test -p kgraf-sandbox --lib 2>&1 | Tee-Object "$r\1-sandbox-lib.txt"
   ```

2. `sh` and `bash` through Git Bash inside the sandbox:

   ```powershell
   cargo test -p kgraf-sandbox --test shell_windows -- --test-threads=1 2>&1 | Tee-Object "$r\2-shell-windows.txt"
   ```

   **2a.** Programs that load `user32.dll` (`whoami.exe`, `where.exe`, PowerShell), `cmd`,
   `sh` and `bash`, and a cleared environment, inside the sandbox:

   ```powershell
   cargo test -p kgraf-sandbox --test startup_windows -- --test-threads=1 2>&1 | Tee-Object "$r\2a-startup-windows.txt"
   ```

   Expected: every test passes except `sh_dash_c_echo_ok_prints_ok` and
   `bash_dash_c_exit_3_returns_3`, and the `sh` and `bash` tests of step 2. Those fail until
   decision 0011 (amendment "Desktop and environment") settles how MSYS2 programs run: their
   output should show `couldn't create signal pipe, Win32 error 5`. If they still show only
   exit `-1073741502` with no message, the desktop fix is not enough: report it, with the
   output of `dumpbin /imports "C:\Program Files\Git\bin\bash.exe"` if Visual Studio is
   installed. The `msys_pipe` and `desktop_access` tests in step 1 measure both causes, so keep
   the step 1 output too.

3. The sandbox escape tests (writes outside the worktree, the user profile, another
   sandbox's process, the registry, a junction, and a real cargo build inside the sandbox):

   ```powershell
   cargo test -p kgraf-sandbox --test escape_windows -- --test-threads=1 2>&1 | Tee-Object "$r\3-escape-windows.txt"
   ```

   **3a.** HTTPS inside the sandbox, with a listener on 127.0.0.1 (no internet needed). The
   test runs three checks: `curl.exe` from System32 (Schannel) outside the sandbox, the curl of
   Git for Windows (`mingw64\bin\curl.exe`, OpenSSL) inside it, and `curl.exe` from System32
   inside it:

   ```powershell
   cargo test -p kgraf-sandbox --test tls_windows 2>&1 | Tee-Object "$r\3a-tls-windows.txt"
   ```

   Expected: all three pass. The Git for Windows test prints a "skipped" line and passes if
   Git for Windows is not found. The System32 test passes because `curl.exe` still fails inside
   the sandbox with `schannel: AcquireCredentialsHandle failed: SEC_E_NO_CREDENTIALS`: Schannel
   refuses a client credential to a restricted token, which every sandboxed command runs with
   (decision 0011, amendment "Schannel"). The sandbox sets `CURL_SSL_BACKEND=openssl` and
   `http.sslBackend=openssl` for git, so git and the curl of Git for Windows work; tools
   whose only TLS stack is Schannel (System32 `curl.exe`, PowerShell, .NET) still fail. If the
   System32 test fails because a handshake arrived, the gap is closed: report it. The four
   `schannel` tests in step 1 measure which token property causes it, so keep the step 1
   output too.

   **3b.** What a command can do to its runner, once `kgraf sandbox setup` has run (the
   sandbox user backend). The command must not open the runner's process, any of its
   threads, or its token for more than waiting and reading; the runner must have no console
   host; the command must still open its own token; and standard input and output must pass
   through the runner:

   ```powershell
   cargo test -p kgraf-sandbox --test runner_windows -- --test-threads=1 2>&1 | Tee-Object "$r\3b-runner-windows.txt"
   ```

   Expected: all five pass, and none prints a "skipped" line (a skipped line means the
   backend is not the sandbox user: send the output of `kgraf sandbox status`). When a
   command cannot start as the sandbox user, the error names the step and, where the
   runner ended before it connected, its exit code with its meaning: `0xC0000142` (a DLL
   failed to start, most likely the window station or desktop), `0x2B470005` (the runner
   could not protect its own process, threads or token), `0x2B470002` (the runner could not
   open its pipe). A failure to put the runner in kGraf's job prints
   `IsProcessInJob(runner) = ...`. Send the full text of any such error.

4. The worker's shell tool on Windows (a plain program and a `sh -c` line run and return
   their output, the network notice comes once, an exit status comes back, a timeout and
   a cancel end the whole process tree), and its pure decisions:

   ```powershell
   cargo test -p kgraf-tools --lib windows -- --test-threads=1 2>&1 | Tee-Object "$r\4-tools-windows.txt"
   ```

5. The run-start notices:

   ```powershell
   cargo test -p kgraf-host --lib network_notice 2>&1 | Tee-Object "$r\5-host-notices.txt"
   ```

6. Optional, longer: every test of the four packages. Some of their tests were written
   for macOS and Linux only and are skipped on Windows; a failure here that is not in
   steps 1 to 5 is still worth reporting.

   ```powershell
   cargo test --no-fail-fast -p kgraf-sandbox -p kgraf-tools -p kgraf-host -p kgraf-bench-briefs -- --test-threads=1 2>&1 | Tee-Object "$r\6-four-packages.txt"
   ```

What to expect: each command ends with `test result: ok.` for each test binary. Write down
any `FAILED` line. A failure in steps 2 to 4 is most useful with its full text, which the
result files keep.

## 5. A short kGraf run that uses the worker shell

This needs one model route. Use any provider you have: a hosted one with its API key in
the PowerShell window (for example `$env:KGRAF_OPENAI_API_KEY = "<key>"` with
`--provider openai`), or a local server such as Ollama for Windows (`--provider ollama`).
The run asks you to approve the shell commands, so stay at the window.

Make a small project to work in:

```powershell
cd C:\src
cargo new kgraf-smoke
cd kgraf-smoke
git add -A
git -c user.name=smoke -c user.email=smoke@example.invalid commit -m "start"
$kgraf = "C:\src\kGraf\target\debug\kgraf.exe"
& $kgraf index
```

On ARM64, the emulated x86_64 build is `C:\src\kGraf\target\x86_64-pc-windows-msvc\debug\kgraf.exe`;
run this step once with each.

Write the brief and run one worker:

```powershell
@'
Use the shell tool for each of these, one call each, then stop and report each output:
1. ["cargo", "--version"]
2. ["sh", "-c", "echo hello from git bash; uname -s; printf '%s|' 'a b' c"]
3. ["git", "status", "--short"]
4. ["cargo", "test"]
Do not change any file.
'@ | Set-Content brief.txt
& $kgraf worker --provider <provider> --model <model-id> --brief brief.txt --events "$r\smoke-events.jsonl" 2>&1 | Tee-Object "$r\7-smoke-run.txt"
```

Answer "allow" when it asks about a command. What to expect:

- The first shell result ends with `[sandbox] network not blocked: the Windows sandbox
  cannot deny network access without administrator rights (decision 0011); shell commands
  run with the network open; run `kgraf sandbox setup` once, with administrator rights, to
  block it (decision 0097)`. Later results do not repeat it. After step 6b, this notice does
  not appear: commands run as the sandbox user.
- Command 2 prints `hello from git bash`, a line starting with `MINGW64_NT` or
  `CLANGARM64_NT`, and `a b|c|`.
- Command 3 prints nothing or a short status, and no error about the git configuration.
- Command 4 builds and runs the new crate's tests (zero tests, `test result: ok`).

If Git Bash is missing, command 2 fails with a message that names Git for Windows and
`KGRAF_GIT_BASH_PATH`; that is the intended behavior, but report it if Git for Windows is
installed.

## 6. cmd and PowerShell scripts under a rule

This checks that a cmd or PowerShell script kGraf cannot read as plain text asks, and that
a plain one runs on its grants. The pure parts (which scripts are unreadable, and that the
text kGraf checks is the text the interpreter gets from the command line) are already in
step 4 of section 4. This step runs the real `cmd.exe` and `powershell.exe`. It needs the
model route and the `kgraf-smoke` project of section 5.

Make a permissions file outside the project for this run only, in Auto with an ask rule:

```powershell
cd C:\src\kgraf-smoke
@'
mode = "auto"
ask = ["shell(curl*)"]
[allow]
programs = ["cmd", "powershell", "echo", "Get-Location", "Write-Output"]
'@ | Set-Content "$r\permissions.toml"
$env:KGRAF_PERMISSIONS_PATH = "$r\permissions.toml"
@'
Use the shell tool for each of these argv lists exactly, one call each, then stop and
report each output or refusal:
1. ["cmd", "/d", "/c", "echo", "plain-cmd"]
2. ["cmd", "/d", "/c", "echo plain-cmd && echo second"]
3. ["powershell", "-NoProfile", "-Command", "Get-Location"]
4. ["cmd", "/d", "/c", "echo", "\"quoted\""]
5. ["cmd", "/d", "/c", "echo two words"]
6. ["powershell", "-NoProfile", "-Command", "Write-Output 'quoted'"]
Do not change any file.
'@ | Set-Content brief-windows-shells.txt
& $kgraf worker --provider <provider> --model <model-id> --brief brief-windows-shells.txt --events "$r\shells-events.jsonl" 2>&1 | Tee-Object "$r\8-windows-shells.txt"
Remove-Item Env:KGRAF_PERMISSIONS_PATH
```

Answer "deny" to every question. What to expect:

- Commands 1, 2 and 3 run without a question: they print `plain-cmd`, then `plain-cmd` and
  `second`, then the folder.
- Commands 4, 5 and 6 each ask, even though every program they name is granted. The
  question says the command cannot be checked against your ask rule `shell(curl*)` and
  why: 4 holds `"`; 5 is one quoted string with no operator; 6 holds `'`.
- If the model changes an argv (for example it splits command 5 into separate words), the
  result does not test that case; note which ones.

## 6a. The Git for Windows check and install offer

Test this on a machine (or a fresh Windows 11 virtual machine) with the built `.msi` or
`.zip`, as a normal user. Each case below starts kGraf and writes what it saw into
`$r\6a-git-check.txt`. `winget` needs Windows 10 1809 or later with App Installer.

1. **Fresh Windows, no Git, winget present.** Confirm: `git --version` and
   `reg query HKLM\SOFTWARE\GitForWindows` both fail. Start `kgraf-app.exe` from the Start
   menu. Expected: one window asks to install Git for Windows with winget and shows the full
   `winget install --id Git.Git ... --scope user ...` line. Answer Yes. Expected: no
   administrator prompt; the install ends; kGraf opens without a restart. Then
   `reg query HKCU\SOFTWARE\GitForWindows /v InstallPath` shows a folder with
   `bin\bash.exe` in it.
2. **Same, answer No.** Reset first: `winget uninstall --id Git.Git`. Start kGraf, answer
   No. Expected: an error window with the download link, the winget command and
   `KGRAF_GIT_BASH_PATH`, and kGraf exits. Nothing was installed.
3. **Same, command line, with a terminal.** Reset. Run `kgraf.exe index` in PowerShell.
   Expected: the same question on the terminal with `[y/N]`; `y` installs and the command
   runs. Run it again with input redirected (`echo | kgraf.exe index`): it must not ask, and
   must stop with the same fix text.
4. **With Git.** Install Git for Windows by hand. Start kGraf. Expected: no question and
   no delay. Repeat with Git installed for all users (`HKLM` key) and with a per-user
   install (`HKCU` key). Repeat with Git installed on a drive other than `C:` and not on
   `PATH`: the registry entry must be enough.
5. **Without winget.** Rename the winget stub for the test:
   `Rename-Item "$env:LOCALAPPDATA\Microsoft\WindowsApps\winget.exe" winget.off`, with Git
   not installed. Start kGraf. Expected: an error window that says winget is not available
   and gives the download link and `KGRAF_GIT_BASH_PATH`; no question. Rename it back.
   Then install Git from the link and start kGraf again: it starts.
6. **Bad override.** Set `KGRAF_GIT_BASH_PATH` to a file that does not exist. Expected: an
   error that names the variable and the file, and no offer to install.

Report, for each case, what the window or terminal said and whether kGraf started.

## 6b. The sandbox user setup

Run in a normal (not administrator) PowerShell:

```
kgraf sandbox status    # before: expect the users and rules to be missing
kgraf sandbox setup     # click Yes in the UAC prompt; each step prints its outcome
kgraf sandbox status    # after: every line present, "logon works" for both users
kgraf sandbox setup     # again: users updated (passwords reset), rules refreshed
kgraf sandbox remove    # UAC again; every item removed
kgraf sandbox status    # everything missing again
```

Run the sandbox check before `setup` and after it (Git for Windows must be installed):

```
kgraf sandbox selfcheck    # before setup
kgraf sandbox setup
kgraf sandbox selfcheck    # after setup
```

Expected before `setup` (token sandbox). The network line is not a pass, and the exit code
is 0:

```
sandbox selfcheck: passed with a gap
  backend: Windows restricted token (token sandbox)
  network: not confined (token sandbox; run `kgraf sandbox setup`)
  hidden folder: not writable
  tree: writable
```

Expected after `setup` (sandbox user). The direct connection to the local origin is blocked,
and the exit code is 0:

```
sandbox selfcheck: ok
  backend: Windows sandbox user
  network: only the egress proxy was reachable
  hidden folder: not writable
  tree: writable
```

Any other output is a failure. A failure starts with `sandbox selfcheck failed (backend: ...)`
and names the step. Send back the full output of both runs.

Also check by hand after `setup`: `Get-LocalUser kgraf-*` lists two users plus two per
worker slot (ten with the default four slots: `kgraf-off-`, `kgraf-on-`, `kgraf-s00-` to
`kgraf-s03-` and `kgraf-o00-` to `kgraf-o03-`), they are not on the sign-in screen, and
`Get-NetFirewallRule -DisplayGroup "kGraf sandbox"` lists four block rules. The rules name
`kgraf-off-` and every `kgraf-sNN-` user, and no `kgraf-oNN-` user. Send back the output of
every command.

After `setup` (run it again if you removed the users), check that commands now run as
the sandbox user. Build kGraf with the runner: `kgraf-sandbox-runner.exe` must sit beside
`kgraf.exe`. Without it, every sandboxed command fails with an error that says so.

1. Run a check or a shell command. The run prints the backend name "Windows sandbox user".
   The token sandbox name means setup did not take effect: send the output.
2. In a sandboxed shell, run `whoami`. Expected: `kgraf-sNN-<hash>`, with the machine
   name before it (`kgraf-off-<hash>` after `setup --slots 0`). A command that needs the
   network (a dependency install) prints `kgraf-oNN-<hash>` (`kgraf-on-<hash>` without
   slots).
3. In a sandboxed shell, run `dir C:\Users\<your name>` (your own profile folder).
   Expected: access denied.
4. Network with the online user (a dependency install, or any command that runs with the
   network allowed): `C:\Windows\System32\curl.exe -sS -o NUL -w "%{http_code}" https://example.com`
   prints `200`. This uses Windows' own TLS (Schannel), which failed under the token sandbox.
5. Network with the offline user (an ordinary shell command): the same `curl.exe` line
   fails to connect. A lookup of a name may still leave the machine (a known gap).
6. In a project's worktree, a sandboxed command can write a file inside the worktree and
   cannot write in a sibling folder, in a top-level `.git` entry, or in a hidden folder.
7. `kgraf sandbox remove` revokes the folder grants it listed, then removes the users. The
   lists are the `sandbox-credentials.grants` file and the `sandbox-credentials.slots`
   folder beside the password store. Then `icacls <a worktree>` shows no entry for a
   `kgraf-` user on the folders you granted, and the `.slots` folder is gone.
8. Worker slots. In two worktrees, start a long command in each at the same time (for
   example `sleep 30; whoami`). The two print different `kgraf-sNN-` users. While both
   run, `icacls <the first worktree>` shows a write entry for the first user only. After
   `setup --slots 0`, the second command waits up to 60 seconds, then fails with an error
   that names `kgraf sandbox setup --slots`. It never prints the first command's user.
9. Two dependency installs at the same time, in two projects, run as two different
   `kgraf-oNN-` users.

The escape tests check the same in the sandbox user phase:
`two_workers_in_different_worktrees_run_as_different_users` (two workers, two users, and
neither writes the other's worktree; with no slots, the second is refused) and
`linking_dependencies_grants_only_the_worktrees_own_user` (a worktree with the checkout's
dependencies linked is not writable by the user of another worktree).

Send back each output, and the text of any error.

## 7. A dependency install in the sandbox

This checks that kGraf runs a project's missing install in its sandbox with the network
open and says so, that the install's own script cannot write outside the project or into
its `.kgraf` folder, and that the consent is asked for. It needs Node.js with npm on `PATH`
(`winget install OpenJS.NodeJS.LTS`) and the network; no model route.

Make a project with one public package and one local package whose install script tries
to write where it may not:

```powershell
cd C:\src
mkdir kgraf-install-check; cd kgraf-install-check
git init -q
mkdir probe
@'
{"name":"probe","version":"1.0.0","scripts":{"postinstall":"node probe.js"}}
'@ | Set-Content probe\package.json
@'
const fs = require('fs');
const out = {};
for (const [k, p] of [['outside', 'C:\\src\\kgraf-escape.txt'], ['state', '..\\..\\.kgraf\\planted.txt']]) {
  try { fs.writeFileSync(p, 'x'); out[k] = 'written'; } catch (e) { out[k] = 'denied ' + e.code; }
}
fs.writeFileSync('probe-result.json', JSON.stringify(out));
'@ | Set-Content probe\probe.js
@'
{"name":"app","version":"1.0.0","dependencies":{"left-pad":"1.3.0","probe":"file:probe"}}
'@ | Set-Content package.json
# Install the local package as a copy, not a link, so its script runs in node_modules\probe.
"install-links=true" | Set-Content .npmrc
& $kgraf deps status 2>&1 | Tee-Object "$r\9-deps-status.txt"
& $kgraf deps install npm 2>&1 | Tee-Object "$r\10-deps-install.txt"
Get-Content node_modules\probe\probe-result.json | Tee-Object "$r\11-probe-result.txt"
Test-Path C:\src\kgraf-escape.txt | Tee-Object -Append "$r\11-probe-result.txt"
Test-Path .kgraf\planted.txt | Tee-Object -Append "$r\11-probe-result.txt"
```

What to expect:

- `deps status` lists `install: npm install in the project root (npm)` with the stored
  answer `unset`.
- `deps install npm` prints `installing `npm install` ...`, then `installed  ran `npm
  install` in the project root in kGraf's sandbox ...; network not limited: the Windows
  sandbox cannot limit network access without administrator rights (decision 0011), so the
  install ran with the network open, and its reads were not confined either; its writes
  were still confined; run `kgraf sandbox setup` once, with administrator rights, to block
  it (decision 0097)`. After step 6b, the install goes through kGraf's proxy instead and
  carries no such notice.
- `node_modules\left-pad` exists. `probe-result.json` shows `outside` and `state` as
  `denied` with an error code (`EPERM` or `EACCES`), and both `Test-Path` lines print
  `False`. A `written` or a `True` is a finding: send it.
- Then check the question: delete `node_modules`, run `& $kgraf deps allow npm` and look
  for `npm: allowed `npm install` in the project root`. `& $kgraf deps reset npm` forgets
  it.

## 8. What to send back

Collect the machine facts into the results folder, then zip it:

```powershell
rustc -vV | Out-File "$r\versions.txt"
cargo --version | Out-File -Append "$r\versions.txt"
git --version | Out-File -Append "$r\versions.txt"
& "$env:ProgramFiles\Git\bin\bash.exe" --version | Select-Object -First 1 | Out-File -Append "$r\versions.txt"
$env:PROCESSOR_ARCHITECTURE | Out-File -Append "$r\versions.txt"
Get-ComputerInfo -Property OsName, OsVersion, OsBuildNumber, OsArchitecture | Out-File -Append "$r\versions.txt"
Compress-Archive -Path "$r\*" -DestinationPath C:\src\kgraf-windows-results.zip -Force
```

Send `C:\src\kgraf-windows-results.zip`, and say which machine it came from (x86_64 or
ARM64, and on ARM64 whether the run used the native or the x86_64 build). Check the files
first: they hold the commands' output and paths on your machine, and the events file holds
what the model wrote. No API key is in them, but look before you send.
