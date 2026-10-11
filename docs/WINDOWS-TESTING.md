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
re-run has five times the binary's expected seconds per test as its limit, at least 120
seconds (`SOLO_RERUN_SECS`) and at most 600 (`solo_limit_for`); a binary gets at most 30 re-runs
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

The artifact holds `report\kgraf-wintest-report-*.md` and `firewall.txt`. The upload step
runs whatever happened before it (`if: always()`), so a job that is cancelled or reaches its
limit still uploads the partial results it has. It bills Windows minutes (twice the Linux
rate on a private repository). The job stops after 150 minutes; the runner stops starting
work earlier than that (see "The time budget" below). A failing test does not fail the
job; a missing report file does. Two things differ from a standard user's machine: the
runner account is an administrator, so the sandbox and firewall checks run elevated, and
the runner has Git for Windows, so the worker shell finds `sh`. A result from the runner
does not replace a run on a standard-user machine without Git for Windows.

Before the run, the workflow does three things, and each one fails the job if it does not
work:

- It copies the bundle to `C:\kgraf-wintest` and gives that folder a protected access
  list: full control for SYSTEM and Administrators, read and execute for Users. The
  sandbox users cannot read the runner account's profile. The checkout folder
  (`GITHUB_WORKSPACE`, on the hosted image usually under `D:\a\`) is not in the profile,
  but its access list comes from the image. The step logs both paths, whether the build
  folder is inside the profile, and the `icacls` lists of the build folder and of the copy.
  The baseline and the sandbox user phase both run from the copy.
- It starts the OpenSSH agent service (`Set-Service ssh-agent -StartupType Manual`, then
  `Start-Service ssh-agent`). The service is off on the hosted image, and an escape test
  probes its pipe.
- It checks that Git for Windows is installed (`C:\Program Files\Git\bin\bash.exe`).

#### The sandbox user phase

The workflow has two inputs. `sandbox_user`, on by default, adds `--sandbox-user` to the
runner. Turn it off with `gh workflow run wintest.yml -f sandbox_user=false`. A person can
add the same flag on any Windows machine: `kgraf-wintest.exe --all --sandbox-user`. `only`,
empty by default, is passed to the runner as `--only <value>` when it is set (see "Running
only some tests" below).

The suite runs first, exactly as without the flag, with no sandbox user set up. That is the
baseline, and it stays comparable with earlier runs. Then the phase runs these steps:

| Step | Command | What it checks |
|---|---|---|
| (b0) | `kgraf sandbox selfcheck` | Before setup, for comparison only. On the token sandbox it passes with a network line and a `dns:` line that are not a pass. |
| (b) | `kgraf sandbox setup --slots 4` | Creates the sandbox users (two per worker slot, one without the network and one with it, plus the two fixed users), the group, the firewall rules and seven WFP filters (three DNS filters and four loopback filters), with no tag. The output lists a received password for every one of these users. A filter Windows refuses does not stop setup: a `refused` line names it with Windows' error and a `probe:` reading, the WFP step's line says `partial`, and setup exits 0 with a closing notice that DNS, loopback connections, or both are not confined on this machine. |
| (c) | `kgraf sandbox status` | Every kGraf line is present, for both users of every slot, the three DNS filter lines and the four loopback filter lines included, except a filter Windows refused at setup, which is `FAILED` with the reason; status then ends with "DNS is not confined on this machine" (or "loopback connections are", or both) and exits 1, and the report lists the `FAILED` line as a problem. Every stored password logs on. |
| (d) | `kgraf sandbox selfcheck` | The gate. It passes only if selfcheck exits 0, prints `sandbox selfcheck: ok` and names the backend `Windows sandbox user`. A pass on the token sandbox means setup did not take effect, so it stops the phase too. If selfcheck failed only on its `dns:` line (every other line passed on the sandbox user), step (e) still runs, and the summary says `DNS FAILED`; `dns: not proved` counts as a failure there, like `dns: not confined`. If selfcheck failed on the sandbox user and printed no result lines (its error names that backend and stops at the probe), step (e) still runs so its tests show which programs start as the sandbox user, and the summary says `GATE FAILED: selfcheck failed on the sandbox user and printed no result lines`. The report then says which of two cases the error line shows: the probe ran and failed the step the line names (an earlier run: "a direct connection to the origin worked"), or the probe did not finish its steps (the line gives only an exit status, for example because Git Bash did not start as that user). Any other failure stops the phase: step (e) does not run. |
| (b2) to (b4) | the fallback grant, `kgraf sandbox-named-object-probe --launch`, `kgraf sandbox status` | Only where the probe in (b) was refused (`session0::Proof::Refused`) and the window is elevated. The phase runs the fallback's elevated action itself, probes again through the fallback and reads the status. See "Named objects" below. |
| (e) | re-run | The sandbox test binaries, the baseline binaries that failed for the cargo jobserver, Git Bash or Schannel, and the end-to-end checks, with the sandbox user active. The runner sets `KGRAF_REQUIRE_SANDBOX_USER=1` and `--show-output` for these binaries (see the rules below). |
| (f) | `kgraf sandbox remove`, then `kgraf sandbox status` | Always runs, also after a failure or Ctrl+C. Every kGraf line must be MISSING, the launcher service line and the fallback grant line included. A service or a grant that is still there is a failure. |

Rules of the phase:

- It uses the sandbox user of the Windows user who runs it (no tag). If that user already has
  a stored sandbox user, the phase does not start, because step (f) would remove it. Run
  `kgraf sandbox remove` first.
- Setup needs administrator rights. The hosted runner is elevated. On another machine, start
  the runner from an elevated prompt, or accept the UAC prompt. The report says so when the
  window is not elevated and a step fails.
- `kgraf-sandbox-runner.exe` must be in `programs\` of the bundle. The phase puts a copy in
  `tests\` for the test binaries, and removes it at the end.
- The sandbox users must be able to read and run the programs in `tests\`. Before setup,
  the phase checks that `tests\` is not inside the profile of the user who runs it, and
  that each program there grants read and execute to Everyone, Authenticated Users or
  Users with no deny entry before that grant. If not, the phase does not start, and the
  report names the sandbox group (`kgraf-sandbox-<hex>`), the folder and the reason. Unzip
  the bundle to a folder such as `C:\kgraf-wintest`, not to Downloads or the Desktop.
- Setup gets `--slots 4` (the default, passed explicitly). The escape tests need two slot
  users or more to check that two worktrees run as two users.
- `KGRAF_SANDBOX_SETUP_TEST=1` stays only for the tagged setup test in the baseline. The
  phase does not use a tag.
- Step (e) sets `KGRAF_REQUIRE_SANDBOX_USER=1`; the baseline suite does not. The runner
  keeps every `KGRAF_` variable of its own environment from the test binaries and sets
  this one only in the environment of the re-run, so a value in the shell that started
  the runner reaches neither half. The baseline is the token sandbox on purpose. With the switch set to 1, a test in `escape_windows.rs`,
  `runner_windows.rs` or `tls_windows.rs` that would take the token sandbox branch, or skip,
  fails instead, and its message says to run `kgraf sandbox setup`. The switch takes `1` or
  `0`. Any other value is an error. Without it, a machine where setup did not take effect
  passes every test and measures nothing.
- The escape tests (`escape_windows`) run with a console of their own, in the baseline and
  in the re-run: the runner starts that binary with `CREATE_NEW_CONSOLE` and still reads
  its output through pipes. `the_sandbox_cannot_attach_to_the_host_console` measures that
  a command cannot attach to its host's console, and a redirected run has no console to
  attach to. In the re-run, with the switch set, that test fails if the binary still has
  no console; it never passes without the measurement. On a desktop an empty console
  window opens while the binary runs.
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
- "Measurements of the re-run": every output line of the re-run that starts with
  `measurement:`, with its binary, its test and the test's time. The runner measures that
  time from the result of the test before it, or from the start of the binary, to the
  test's own result: with one test thread a test starts when the one before it ends. The re-run passes `--show-output`, so libtest prints
  the output of passing tests in its summary. A binary that hung or crashed never prints
  that summary: the section names each such binary, because the lines of its passing
  tests are missing.
- The end-to-end checks with the baseline status and the sandbox user status.
- The confirmation after (f). If anything is left, the runner exits with a non-zero code.

The "Tests" and "End-to-end checks" rows of the first table count the baseline. The verdict
line counts both halves (`report::headline`):

- every baseline failure, except those listed in "Expected under the token sandbox";
- every failed test and every hung or crashed binary of the re-run (e);
- every failed end-to-end check, of the baseline and of the sandbox user.

A baseline failure is expected under the token sandbox only when both hold: the same test
passed in the re-run (a pass in the binary's first run; a pass only in a solo re-run does
not count), and its own baseline output names a token sandbox limit. The limits are the
markers that select a binary for the re-run (the cargo jobserver, Git Bash, msys,
`NtCreateDirectoryObject`, the signal pipe, Schannel) and `STATUS_DLL_INIT_FAILED`
(0xC0000142, also printed as -1073741502 or 3221225794), with which Git Bash's programs
and rustc end under the token sandbox. Those failures are not in the verdict; the section
"Expected under the token sandbox (N)" lists each with its binary, its test and the marker,
and the verdict says how many it listed apart. A baseline failure without a marker in its
own output, or one whose test failed, hung or was not reached in the re-run, still counts,
as does a failure in a binary the re-run did not take. The Failures section also lists each
failure of the re-run, with its output, under `sandbox user:`.

When selfcheck (d) failed on its `dns:` line, a line `Known gap: DNS is not confined on this
machine ...` follows the verdict, with selfcheck's words. It is not a pass: the DNS test of
the re-run (`an_offline_lookup_does_not_reach_the_dns_client_service`) fails while the gap
stands, and that failure counts.

#### The time budget (`--budget-mins`)

A job that a host kills at a fixed limit leaves no report. `kgraf-wintest.exe --budget-mins
<N>` gives the run N minutes of wall-clock time, counted from the start of the runner, for
everything: the baseline, the sandbox user phase, the removal of the sandbox user, the cleanup
and the report. Without the option there is no budget.

How the runner keeps to it:

- Before it starts an item, in the baseline, in the sandbox user phase and in the re-runs, it
  compares the item's expected seconds (the manifest's measured rate, the same number the
  progress estimate uses) with the time left. An item that does not fit is not started. The
  report lists it under "Not run" as `not run: budget (expected about N s, M s left)`. The
  end-to-end checks and the whole sandbox user phase are items too. The phase needs 300 s
  (`PHASE_START_SECS`) for setup and the gate; with less left it does not start, and nothing
  is changed on the machine.
- The time limit of a running item is cut to the time left. When the budget ends the item,
  the existing kill path ends its process tree, and the report records it as `stopped at the
  budget` (JSON: `budget_stopped`; it is not `timed_out`, because the item's own limit was not
  the cause). No solo re-run follows a stop. A solo re-run that the budget ends reads
  `stopped at the budget`, not `hung`.
- A reserve is kept free at the end: 540 s with a sandbox user phase (the removal's limit of
  300 s, the status check's 120 s, and 120 s for the cleanup and the report), 120 s without.
  Work never uses it. The removal, the cleanup and the report run after the budget is spent,
  and the removal runs even when the budget ended the phase early.
- The verdict reads `INCOMPLETE: the time budget of N min ran out; ...` as soon as one item did
  not run or was stopped. It never reads `ALL PASSED`; the failures that did happen are still
  counted in the same line. The summary has a "Time budget" row, and the runner exits 1.

The workflow computes the budget from the job: the job limit (150 minutes), less the minutes
used when the bundle step starts, less 12 minutes for the steps after it, less 1. The steps
before the bundle took 20 to 22.5 minutes and the steps after it about 9 (the cache save alone
7 minutes), so the runner gets about 116 minutes, and the budget line is printed in the step's
log.

#### Reading the progress line

A step that runs a named item shows the item in place of the step name, without the bar and
the estimate: `<percent> <done>/<total> <clock> | re-run <binary id> test 3 of 12 | ...`.
Each re-run in the sandbox user phase shows `re-run <binary id>`, and a solo re-run shows
`solo <binary id>::<test name>` (in the baseline and in the phase). The log also gets a line
`sandbox user: running <binary id> again` when a re-run starts and `solo re-run
<binary id>::<test name>` for a solo re-run, so a stall is attributable from the log alone.

#### Running only some tests (`--only`)

`kgraf-wintest.exe --only <filter>` runs only what the filter names, in the baseline suite
and in the sandbox user re-run alike. The filter is a comma-separated list of items:

- `<run id>`: one test binary, whole. Run ids are the ones in the report's "Test binaries"
  table, for example `kgraf-sandbox/test/escape_windows`. An id without its `#core` or
  `#rest` part names both parts.
- `<run id>::<test name>`: one test of that binary, by its full name as the report prints
  it, for example
  `kgraf-bench-briefs/lib/kgraf_bench_briefs::check::tests::verify_fails_when_a_protected_file_changed`.
  The named tests of one binary run together with libtest `--exact`. Name the part
  (`#core` or `#rest`) of a split binary to run a test once.
- `e2e`: every end-to-end check. `e2e::<check id>` (for example `e2e::shell.git_bash`)
  keeps that check; the checks run in groups that share one worker run, so its group runs
  and only the named check is reported. Without an `e2e` item no end-to-end check runs.

An item that names no binary of the bundle stops the runner before it starts anything, and a
named end-to-end check that did not run is reported as a failed check. With a filter, the
runner asks nothing about the extended set, and in the sandbox user phase every binary the
filter named runs again, whatever the baseline found. The report states the filter in a
line above the verdict and in the "Mode" row. An empty value is the full run, so the
workflow passes its `only` input through as it is:

```
gh workflow run wintest.yml -f only='kgraf-sandbox/test/escape_windows::send_input_reaches_nothing_of_the_owners'
```

#### What a confirmation run must show

A confirmation run is a full run (no `only`) with the sandbox user phase. It reads clean
when:

- The verdict line counts no failed test, no hung or crashed binary and no failed
  end-to-end check, except what a known gap explains, and the report names each such
  failure. While the DNS gap stands, that is the re-run's
  `dns_windows::an_offline_lookup_does_not_reach_the_dns_client_service` with the
  `Known gap: DNS ...` line. The baseline's end-to-end check `shell.git_bash` fails under the
  token sandbox by design (Git Bash cannot make its object directory) and is counted; its
  sandbox user status must be `Pass`.
- "Expected under the token sandbox (N)" lists the baseline's Git Bash, jobserver and
  0xC0000142 failures, and each one's sandbox user status is `passed`.
- In (d): `launch: kGraf's launcher service made window station kgraf-cmd-... and desktop
  Default for the command of sandbox user ... in session 0`, and a `launch: the session-0
  path started a command: ... for a command on kgraf-cmd-...\Default` line for each command.
- In (e): `escape_windows` (`send_input_reaches_nothing_of_the_owners`,
  `the_command_runs_on_its_own_desktop_without_a_console`,
  `user_interface_objects_are_shared_the_way_the_backend_shares_them`),
  `runner_windows::the_command_runs_on_a_desktop_the_owner_made`,
  `startup_windows::a_cleared_environment_keeps_the_base_and_the_sandbox_settings_only`,
  `kgraf_bench_briefs` `check::tests::verify_fails_when_a_protected_file_changed`,
  `kgraf_host#rest` `tests::everything_a_check_creates_is_removed_and_nothing_outside` and
  every test of `contract_checks` pass, and no binary of the re-run timed out.
- (f) leaves nothing.

To re-run only the tests these fixes address, pass this `only` value:

```
kgraf-sandbox/test/escape_windows::send_input_reaches_nothing_of_the_owners,kgraf-sandbox/test/escape_windows::the_command_runs_on_its_own_desktop_without_a_console,kgraf-sandbox/test/escape_windows::user_interface_objects_are_shared_the_way_the_backend_shares_them,kgraf-sandbox/test/runner_windows::the_command_runs_on_a_desktop_the_owner_made,kgraf-sandbox/test/startup_windows::a_cleared_environment_keeps_the_base_and_the_sandbox_settings_only,kgraf-orchestrator/test/contract_checks,kgraf-bench-briefs/lib/kgraf_bench_briefs::check::tests::verify_fails_when_a_protected_file_changed,kgraf-host/lib/kgraf_host#rest::tests::everything_a_check_creates_is_removed_and_nothing_outside
```

What the phase collects about the launch path, in the output of selfcheck (b0) and (d):
after the report lines, selfcheck prints its launch record on standard error, each line
starting with `  launch:`. On the sandbox user (d) they are: one line for each window
station and desktop kGraf made (`made window station kgraf-run-... and desktop Default for
the runner of sandbox user S-1-5-21-...` and the same with `kgraf-cmd-...` for the
command; on the session-0 path, `kGraf's launcher service made window station kgraf-cmd-...
and desktop Default for the command of sandbox user S-1-5-21-... in session 0`), the logon calls (`CreateProcessWithLogonW calls N (F failed): last ..., mean ...,
longest ...; spawn lock wait before them: ...`), and the spawn lock (`spawn lock, every
spawn in this process: taken N, waits that ran out T; wait mean ..., longest ...; longest
hold ...`). The report keeps them with the rest of the output. Read the station lines
first when the gate fails: a missing `kgraf-cmd-` line with an error naming
`CreateWindowStationW` or `CreateDesktopW` and a Win32 error means the command's station
could not be made; an error naming `OpenWindowStationW` means the runner could not open it.
The longest logon call is what every other spawn may wait for. `runner_windows` and
`stdio_windows` are not among the binaries the phase re-runs; run them by hand (3b, 3c)
to collect their lines.

What the phase collects about the loopback filters, the jobserver and git, and what to read
in the report (each came from the first self-test run whose re-run ran as the sandbox user):

- Loopback. That run had the firewall rule `block-loopback-tcp` present, and the offline slot
  user still reached a listener on 127.0.0.1 outside kGraf's proxy ports: selfcheck (d)
  failed with "a direct connection to the origin worked", and
  `a_command_without_the_network_reaches_the_proxy_ports_and_nothing_else` failed with
  "reached a local listener ... connected". Windows Firewall rules do not act on that
  loopback traffic. Setup now adds four filters at the connect layers
  (`FWPM_LAYER_ALE_AUTH_CONNECT_V4` and `_V6`): one per IP version for every port outside
  the proxy ports, one per IP version for UDP. Expect in (b) a `created` WFP step line and no
  `refused` line for a loopback filter; in (c) four lines `present  loopback filter on the
  connect layer, IPv4 ...` and `..., IPv6 ...`; in (d) the line `network: only the egress
  proxy was reachable`; and in (e) that escape test passing. A `refused` line or a `FAILED`
  loopback line means Windows refused that filter shape: send the line whole with its
  `probe:` reading.
- The jobserver. That run stopped every cargo build as the sandbox user at rustc's start
  ("failed to create jobserver: Os { code: 5 }", exit 0xC0000409): rustc could not create
  its named semaphore in the session. kGraf now makes one per command and names it in
  `CARGO_MAKEFLAGS`. Expect in (d) a launch line `made jobserver semaphore kgraf-jobserver-...
  with N tokens for a command of sandbox user ...`. The next run refused the open of that
  semaphore too; see "Named objects" below for the launcher service that is meant to fix
  this, and for what (e) must show now. The cargo test prints cargo's own lines. A line
  `warn: could not canonicalize path C:\Users\<user>` from rustup is expected: rustup looks for
  a toolchain override in each folder above the command's folder, and the owner's profile
  folder is not readable to the sandbox user. It is a warning, not a failure.
- Git. That run's `git status` as the sandbox user failed with "detected dubious ownership":
  the worktree belongs to the account that runs kGraf. A command that runs as the sandbox user now gets
  `safe.directory` entries for its roots through `GIT_CONFIG_COUNT`. Expect the end-to-end
  check `shell.git` and `a_worker_runs_end_to_end_on_the_real_tools_transcript_masking_mcp_and_ledger`
  to pass in (e).
- The read grant on a repository's `.git`. That run's notes said "the read grant on
  ...\.git reached N of its files and folders; ... could not be opened or written",
  each an object file git writes read-only. Such a file now gets a second, minimal open and a
  lone write. Expect no such note in (e); a note that remains names the step that failed
  (`open:`, `read:`, `write (SetSecurityInfo):` or `write (lone):`), which decides the next
  change.
- The worktree hold. `a_worktree_the_worker_still_writes_is_not_snapshotted` must fail the
  verification with "still runs": kGraf now lists each command it started in a worktree by
  its job, and quiesce is busy while one still holds a process.

What the phase collects about named objects, git's ref writes, the junction test and DNS,
and what to read in the report (each came from the second self-test run whose re-run ran as
the sandbox user; none of the changes has run on Windows since):

- Named objects. The second self-test run proved that a sandbox user's command in the
  owner's session can neither make nor open a named object: "measurement: a named semaphore
  in the session as the slot user without the network: exit 5, denied", and cargo's "cannot
  open path or name kgraf-jobserver-... Access is denied (os error 5)". rustc then aborted
  with "failed to create global mutex named `rustc_errors`" and exit 0xC0000409. About forty
  cargo tests failed this way (bench-briefs, contract checks, contract flow, four
  kgraf_host tests, the cargo escape test). The cause, as far as the evidence goes:
  `kernelbase.dll` opens the session's object directory itself, with explicit rights,
  before it looks up any name, and that directory admits the session's logon SID, which the
  command holds deny-only. kGraf now starts a sandboxed command in session 0 through a
  launcher service that setup installs, and keeps a recorded grant on the owner's session
  object directory as the fallback (decision 0097, the last amendment). The probe decides
  which path is active. Nothing of this has run on Windows. The next run must show the
  following, in this order. Send each line whole.
  1. The Windows session of the process that runs kGraf. It is the first line of the sandbox
     user phase in the report, before any verdict, and it is read with
     `ProcessIdToSessionId`. On a hosted machine that process may be in session 0. Then both
     paths name the same machine-wide object directory, and every result below must be read
     that way: a named object made there does not show that a command works from an
     interactive session.
  2. The launcher service. Setup (b) installs it. Status (c) has a line `present  launcher
     service kgraf-launcher-<hash> (installed, stopped; started when a command needs it)`,
     and the description of the service reads back as kGraf's. A stopped service between
     commands is normal: it starts on demand and is not a gap.
  3. The `launch:` lines. Each ask of the service gives a line `launch: the session0 path
     started a command: ... (at ...)` that names the path that started the command and, for
     a logon that failed, its logon type and Win32 error. The report lists these lines, and
     the `named objects:` lines, in the "Launch path" section.
  4. The `named objects:` line in (b), (c) and (d). It names the active path, why that path
     is the active one, and what the probe found. A probe that made a named semaphore and an
     unprefixed mutex says "Proved". A probe that ran and was refused says "The proof
     failed, a named object was refused". A probe that could not run says so, and that it
     says nothing about named objects.
  5. The `measurement:` lines for a named semaphore and for an unprefixed mutex, each made
     by a sandbox user through the active path, and the jobserver opened under its `Global\`
     name (`Global\kgraf-jobserver-<hex>` on the session-0 path). The test
     `the_sandbox_user_opens_its_jobserver_and_named_objects_are_measured` must pass, or fail
     with a message that names the cause.
  6. cargo and rustc running as a sandbox user. The tests that failed on the named object
     (the cargo tests listed above) must pass now, or fail for a reason that is named. The
     environment of a command comes from `CreateEnvironmentBlock` and the user profile is not
     loaded, so what a build reads from `HKEY_CURRENT_USER` is not available. The run shows
     whether cargo, rustc or a toolchain lookup needs it. A failed cargo command's output
     still ends with a `[sandbox]` line that names the cause where cargo cannot run.
  7. No change in what the launcher touches: standard input and output, the exit status, the
     kill when the command is dropped, the time limits, the job, the egress proxy and the
     network filters (the WFP filters match by user SID). The tests for these must pass as
     they did before.
  8. A clean remove (f). The service is stopped and deleted, the fallback grant is taken off
     where it was made, and nothing of kGraf's is left. The status after the removal has the
     line `MISSING  launcher service ...` and a `MISSING` line for the fallback grant. The
     report says "Launcher service after the removal: gone. Fallback grant after the
     removal: gone." A service or a grant that is still there, or a status that prints
     neither line, makes the removal check fail.
  9. If the probe in (b) was refused, the phase exercises the fallback by itself, with no
     person at the keyboard. After setup it runs the elevated action
     `kgraf sandbox elevated --action grant-named-objects` (step (b2), started from the
     runner the way setup starts its own elevated step), probes again through the fallback
     (step (b3), `kgraf sandbox-named-object-probe --launch`) and reads the status (b4). The
     report section "Named objects: the fallback and the path chosen" prints, in this order:
     the Windows session the elevated step ran in; the access list of that session's object
     directory before the grant and after it; the path chosen (session 0, the fallback grant,
     or neither); and the probe outcome for each path tried, in the words of
     `session0::Proof` (`Made`, `Refused`, `CouldNotRun`). Where the fallback is the active
     path, the whole list above must hold through it, and the risk line must be in setup,
     status, selfcheck and the report: a sandboxed command can then put a named object in
     the directory the owner's own programs read. The grant is gone at the next sign-out in
     any case, and remove takes it off as well. If the first probe was not refused, the
     report says the fallback did not run.
  10. Commands from a program other than `programs\kgraf.exe`. A test binary in `tests\`
     starts its commands through the runner the launcher service was installed with, which
     the owner's side reads from the service's image path: the one in `programs\`, never the
     copy the phase puts beside the test binaries (that copy serves the owner-session path
     alone). The sandbox user re-run of the shell, startup, escape, runner, stdio and
     contract tests must not fail with "check where kGraf's launcher service starts its
     program from". Where the service is missing, unreadable or set up for another user or
     pipe, every command fails with one line that names the service and ends "run `kgraf
     sandbox setup` again", and no command runs on another path. A refused launch says
     "sandbox setup failed at" and "refusing to run unconfined" once each. The sandbox crate
     tests `the_pipes_access_list_is_accepted_by_windows` and
     `the_creation_attributes_bound_inheritance_and_join_the_job` pass: a test runs as the
     owner, and the owner is refused another instance of the launcher pipe with error 5,
     which is the result the first of them expects.
  If the process that runs kGraf is in session 0, items 4, 5 and 9 describe one machine-wide
  directory and prove less than they do from an interactive session. Say so when you send the
  report.
- Git's ref writes. That run's `everything_a_check_creates_is_removed_and_nothing_outside`
  failed in a check that ran `git init` in a tree in the owner's profile: "cannot lock ref
  'HEAD': unable to create directory for .../nested/.git/HEAD". Git stats each folder of
  the repository's absolute path from the drive root before it writes a ref, and the sandbox
  user cannot read the attributes of a folder in the owner's profile. Expect the test to
  fail the same way, and in (e) the line `measurement: the attributes of each folder above
  the tree ...: exit 5, denied ...<the first folder that refuses>` from
  `the_attributes_of_each_folder_above_the_tree_are_measured`. Exit 0 there with the test
  still failing means the cause is another one: send the test's tail whole.
- The junction test. It now checks both halves of the link walk: beside the junction its
  probe planted, the next command of the same user starts (that user's entry is already on
  the tree, so nothing is written and nothing can be carried through the link) and the
  target's access list is unchanged; on a fresh tree holding a junction out of it, the first
  grant for each user is refused with "holds the link ... nothing was written". Expect
  `a_junction_inside_the_root_cannot_escape` to pass.
- The worker end-to-end test now expects Git Bash's "Permission denied" for the denied read
  on Windows. Expect
  `a_worker_runs_end_to_end_on_the_real_tools_transcript_masking_mcp_and_ledger` to pass.
- DNS. `an_offline_lookup_does_not_reach_the_dns_client_service` still fails while Windows
  refuses the name resolution cache filters. Its message now says "This is the recorded
  gap" when a name resolution cache filter is absent or disabled, and calls the leak a
  defect of the filters when every one is present and running.

What the phase collects about the DNS filter, and what to read in the report:

- The `dns:` line of selfcheck before setup (b0) and after it (d). Before: `dns: not
  confined (token sandbox; ...)`. After: `dns: confined (...)`. Any other line after setup
  means the filters are not proved on that machine; the summary then says `DNS FAILED`.
  A `dns: not proved` line names the status the probe got: a timeout, a status the check
  does not know, or output cut off before the probe's `KGRAF-DNS-END` line. Send that
  status back; it decides whether the next change is a longer wait or another layer.
- The three DNS filter lines of status (c) and of status after removal (f): `present`
  (or `FAILED`, for a filter Windows refused at setup), then `MISSING`. There is one line
  per filter, each naming its layer (the user-mode RPC layer, and the name resolution cache
  layer for IPv4 and for IPv6) and its own key.
- For each refused filter, the `refused` line of setup (b). Its reason carries Windows'
  status (Windows Server 2025 answered `FWP_E_OUT_OF_BOUNDS` for the IPv4 name resolution
  cache filter) and, for a name resolution cache layer, a `probe:` reading: setup added two
  permit filters at that layer, one with no condition and one with kGraf's user condition,
  each in a transaction it aborted, and the reading says which of them Windows took. It
  tells whether the layer refuses any static filter, the user condition, or the block
  action. Send the line whole; it decides what the next change to the DNS filter is.
- The test binary `dns_windows`, in the baseline (each test returns at once, because no
  sandbox user is set up) and in the re-run (e). In the re-run it checks the
  `netsh wfp show filters` entry of each filter that exists (name, layer
  `FWPM_LAYER_NAME_RESOLUTION_CACHE_V4`, `FWPM_LAYER_NAME_RESOLUTION_CACHE_V6`,
  `FWPM_LAYER_ALE_AUTH_CONNECT_V4` or `FWPM_LAYER_ALE_AUTH_CONNECT_V6`, block), with a
  `measurement:` line for each filter it does not look for: one Windows refused at setup,
  and the filter on `FWPM_LAYER_RPC_UM`, which netsh does not list (an earlier run: present by
  kGraf's own reading, absent from the netsh list). Then the cache check: after
  `Clear-DnsClientCache`, a lookup by the offline user fails and `Get-DnsClientCache` does
  not list its name, while the online user and the owner still resolve. That test prints a
  `measurement:` line per DNS filter (present, absent or disabled) and judges by the lookup
  alone: on a machine that refused the name resolution cache filters (Windows Server 2025)
  the lookup leaves the machine and the test fails on it, which is the recorded gap. A
  failure message carries the netsh entry or the lookup status.
- Read the failed tests of `dns_windows` in the re-run table before the selfcheck verdict:
  they say which part did not hold (the filter's listing, the offline lookup, or the
  online one).

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
   cargo test -p kgraf-sandbox --lib -- --nocapture 2>&1 | Tee-Object "$r\1-sandbox-lib.txt"
   ```

   `--nocapture` is needed for the `MEASURE` lines. Search this file for `MEASURE` and
   send every line. Four questions ride on them, all about the access entries kGraf
   writes on a tree it grants a sandbox user:

   - `MEASURE maximum-allowed-suppresses-propagation: suppressed true|false`. kGraf
     writes a folder's own list through a handle opened with `MAXIMUM_ALLOWED`, which
     Microsoft documents as not propagating entries to children, and then writes each
     existing child itself. A `false` here means Windows propagated anyway on this build,
     so kGraf's entries reach children twice (harmless) but a hard-linked file gets the
     entry after all (not harmless: the gap this closes).
   - `MEASURE grant-propagation-follows-junction: target folder ... file in it ...`. A
     `true` means Windows' own propagation enters a junction, which is what the link walk
     in front of every write exists to stop.
   - `MEASURE first-grant-pass: ... folders, ... files, ... children written, ... ms`. The
     cost of kGraf's own pass. The kernel's propagation opens each child too, so the pass
     replaces that cost rather than adding one; this line says what it is on real
     hardware. A worktree with a built `target` folder holds far more files than this
     test's tree, so scale the number.
   - The tests `a_hard_linked_file_in_the_tree_never_gets_the_grant` and
     `the_removal_takes_the_entry_off_every_existing_child` run only here. The first says
     whether a hard-linked file keeps its own list, so a command cannot write it; the
     second whether a removal takes kGraf's entry off every child. Send both results, and
     any note that names a file a grant left alone: a build that writes such a file in
     place fails with an access error.
   - The tests `a_lone_write_on_a_held_folder_changes_no_child`,
     `a_grant_and_a_revoke_succeed_with_folders_held_without_delete_sharing` and
     `a_revoke_through_a_lone_write_drops_the_inherited_copy` open a folder of the tree
     themselves with a handle that does not share delete (`CreateFileW`,
     `FILE_FLAG_BACKUP_SEMANTICS`), as other programs held `C:\` and `C:\Windows\Tasks`.
     A process's current folder does not do this: on Windows Server 2025 a folder that
     `ping.exe` had as its current folder still opened with `MAXIMUM_ALLOWED`. Each
     first checks that an open with `MAXIMUM_ALLOWED` is refused there with os error 32;
     a panic that says "the condition under test is absent" means this build does not
     refuse it, and the other results say nothing. Otherwise the first says whether the
     write that replaces the refused open changes no child, the second whether a grant
     and a revoke succeed with the root and a folder below it held, and the
     third whether a revoke on such a folder also drops the entry it had inherited.
   - The tests `closing_a_folder_changes_no_child_and_keeps_the_protection` and
     `a_protected_folder_keeps_its_protection_and_its_list` write a folder's list alone and
     read it back. On Windows Server 2025 both failed with "protection bits 0x0400 before,
     0x0000 after" and "0x1400 before, 0x1000 after": the write dropped the
     auto-inherited bit. The write now also sets `SE_DACL_AUTO_INHERIT_REQ` for a folder
     that had the bit. A pass says that keeps it; the same failure says it does not, and
     the step stays refused with the old list written back where Windows allows it. A
     failed precondition in the third ("the folder holds an inherited copy") means a
     folder written by the grant pass took in no inherited entry on this build; send
     the line.

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

   The probe is the test binary itself. A sandbox user cannot read the owner's profile, so
   the build output must be outside it, or every probe dies with no result. Set
   `CARGO_TARGET_DIR` to a folder outside the profile (for example `C:\kgraf-target`) before
   you build and run the Windows test binaries, here and in steps 3a and 3b:

   ```powershell
   $env:CARGO_TARGET_DIR = "C:\kgraf-target"
   ```

   The tests fail loud with a message that names `CARGO_TARGET_DIR` when the binary is under
   the profile. To test the sandbox user (after `kgraf sandbox setup`), also set
   `$env:KGRAF_REQUIRE_SANDBOX_USER = "1"`. Leave it unset for the token sandbox.

   The tests branch on the backend in use. Several report a measurement, not the block of an
   attack. A run must collect these:

   - `the_strength_indicator_matches_the_backend_in_use`: which backend the run measured.
   - `a_write_into_an_everyone_writable_directory_matches_what_the_backend_claims`: whether a
     sandbox user can write an Everyone-writable folder under ProgramData. The gaps text of
     the sandbox user backend says it can. A denial means the claim must be narrowed.
   - `reads_outside_the_workspace_are_not_confined`: whether a sandbox user reads a file
     under ProgramData (outside the owner's profile).
   - `a_command_without_the_network_reaches_the_proxy_ports_and_nothing_else`: three
     results, in this order. A listener on a port outside 47100-47131 is unreachable. A
     listener on a port inside that range is reachable (the positive control: without it the
     first result proves nothing). An address outside loopback (192.0.2.1:80, TEST-NET-1) is
     unreachable.
   - `a_unc_path_outside_loopback_is_measured_not_blocked`: a measurement, not a block. The
     test prints lines that start with `measurement:` for `\\192.0.2.1\x` and
     `\\192.0.2.1@80\x`. Record those lines with how long the test took. Traffic
     that the kernel's SMB client or the WebClient service sends does not run as the sandbox
     user, so the firewall rules scoped to that user do not cover it. Nothing in the test
     watches the wire.
   - `one_sandbox_cannot_open_another_sandboxs_process_for_write`: three outcomes, each with
     its own meaning. Access denied: the two folders ran as different users. The open
     succeeded: a defect. The second command was refused a sandbox user of its own because
     setup made no slot users: also confinement.

   - `the_command_runs_on_its_own_desktop_without_a_console` and
     `user_interface_objects_are_shared_the_way_the_backend_shares_them`: the window station
     name of each command. As a sandbox user kGraf's process makes one station for the
     commands of each sandbox user, `kgraf-cmd-<hex>`, with one desktop called `Default` on
     it: two commands in one worktree run as one user and share it, two worktrees run as two
     users on two stations.
   - `send_input_reaches_nothing_of_the_owners`: how many events `SendInput` injected, and
     the station it ran on, in one line from one command. A count above zero on a private
     station reaches nothing, so the count is a measurement there, not a failure.
   - `the_ssh_agent_pipe_cannot_be_opened`: whether a command opened the agent's pipe. As a
     sandbox user that is a measurement: the pipe admits authenticated users, and the gaps
     text concedes that named objects every user may write are not closed. Start the agent
     service first, or the test fails with the switch set.
   - `a_hard_link_to_an_outside_file_cannot_escape`,
     `a_junction_inside_the_root_cannot_escape` and
     `the_owner_cannot_rewrite_the_dacl_of_an_outside_file`: each runs a second phase
     against an Everyone-writable folder under ProgramData, where the target's own access
     list decides and not the owner's profile, and prints a `measurement:` line. The
     junction test also checks, as a sandbox user, that the next command of the same user
     beside the planted junction starts with the target's access list unchanged, and that
     on a fresh tree holding a junction the first grant for each user is refused before any
     access entry is written ("holds the link ... nothing was written").
   - `the_sandbox_user_opens_its_jobserver_and_named_objects_are_measured`: a sandbox user
     opens the jobserver kGraf made for its command; `measurement:` lines give the owner's
     reading of the session's object directory and of its `Restricted` subdirectory, and
     per kind whether a named semaphore of the command's own may be made in the session and
     in the global namespace, and whether the directory opens with the rights
     `kernelbase.dll` asks for and for traversal alone.
   - `the_attributes_of_each_folder_above_the_tree_are_measured`: a `measurement:` line per
     kind naming the first folder above the tree whose attributes the sandbox user cannot
     read, which git needs before it writes a ref.

   In a manual run, add `--show-output` to the command above (libtest prints the output of
   a passing test only then) and copy every line that starts with `measurement:` into the
   report. The self-test runner does this in the re-run of its sandbox user phase (section
   0). The tests fail where a thing they probe is absent and `KGRAF_REQUIRE_SANDBOX_USER=1`
   is set: the OpenSSH agent service (`Start-Service ssh-agent`), a console window for the
   test binary (run it from a PowerShell window, not with its output redirected by a
   program that gives it no console), Git for Windows for the TLS test, and two offline
   slot users for the linked-dependency test (`kgraf sandbox setup --slots 2` or more).
   Start or install each one before the run instead of reading a skip as a pass.

   The probes run under both kinds of sandbox user where it matters (the slot user without
   the network and the one with it), so a failure message names which kind. The runner tests
   (step 3b) check `PROCESS_VM_READ` and `PROCESS_VM_WRITE` against the runner process, so
   the escape tests do not repeat them.

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
   host; the command must still open its own token; standard input and output must pass
   through the runner; and the command must run on a window station and desktop that
   kGraf's own process made for it (the runner makes none):

   ```powershell
   cargo test -p kgraf-sandbox --test runner_windows -- --test-threads=1 --nocapture 2>&1 | Tee-Object "$r\3b-runner-windows.txt"
   ```

   Expected: all seven pass, and none prints a "skipped" line (a skipped line means the
   backend is not the sandbox user: send the output of `kgraf sandbox status`).
   `the_command_runs_on_a_desktop_the_owner_made` prints the launch record, lines that
   start with `launch:`. Send them all:
   `launch: made window station kgraf-run-... and desktop Default for the runner ...` and
   `... kgraf-cmd-... for the command ...` (one each per sandbox user: the stations kGraf
   made, and that each creation worked), `launch: CreateProcessWithLogonW calls N (F
   failed): last ..., mean ..., longest ...; spawn lock wait before them: ...` (how long the
   logon call takes on this machine; every other spawn of the process waits for it), and
   `launch: spawn lock, every spawn in this process: ...` (the longest wait of any spawn for
   the lock; one near 60 s means spawns queue behind logons for too long). If the command's
   station cannot be made, the error names the call and the Win32 error, for example
   `make the command's window station (CreateWindowStationW): Access is denied. (os error
   5)`; if the runner cannot open it, `open the command's window station
   (OpenWindowStationW)` with the error. Send the full text.
   `the_runners_lists_are_written_before_it_runs_and_read_back` prints one line,
   `runner lists before resume: ...`. `Some(Ok(()))` means kGraf wrote the protected
   access lists on the runner's process and main thread while it was still suspended,
   and read them back. `Some(Err(...))` is a measurement, not only a failure: it holds the
   step and the Win32 error of that write on this machine (the rights of the handles
   `CreateProcessWithLogonW` returns are not documented). The command still ran, with the
   runner protecting itself as its first step. Send the line either way. When a
   command cannot start as the sandbox user, the error names the step and, where the
   runner ended before it connected, its exit code with its meaning: `0xC0000142` (a DLL
   failed to start, most likely the window station or desktop), `0x2B470005` (the runner
   could not protect its own process, threads or token), `0x2B470002` (the runner could not
   open its pipe). A failure to put the runner in kGraf's job prints
   `IsProcessInJob(runner) = ...`. Send the full text of any such error.

   **3c.** A command's standard handles, on whichever backend is active. Run it before
   `kgraf sandbox setup` (token sandbox) and again after it (sandbox user):

   ```powershell
   cargo test -p kgraf-sandbox --test stdio_windows -- --test-threads=1 --nocapture 2>&1 | Tee-Object "$r\3c-stdio-windows.txt"
   ```

   Expected: all four pass.
   `a_concurrent_tokio_spawn_leaks_no_pipe_into_a_sandboxed_commands_start` starts three
   sandboxed commands that live about 12 seconds while a thread starts short tokio children
   through the spawn lock, and prints `3 sandboxed commands beside N tokio spawns; slowest
   ...` and the `launch:` lines. Send them. A failure (`an unrelated tokio child took ...`)
   means a sandboxed process, on the sandbox user the runner, took a copy of that child's
   pipe end during its start.
   `a_concurrent_unrelated_spawn_does_not_hold_a_sandboxed_commands_output` runs 20
   sandboxed commands while four threads start unrelated processes that live about five
   seconds each, and prints `20 sandboxed commands beside N unrelated spawns; slowest
   ...`. Send that line. A failure names a command whose output pipe stayed open after it
   ended: an unrelated process took a copy of its pipe end. `a_commands_standard_input_is_never_a_console`
   and `an_inherited_console_as_standard_input_is_refused` check that a command never gets
   a console as its standard input; the second briefly opens a console window of its own.

   Every wait for a sandboxed command now ends. If a test or a kGraf run stops with one of
   these errors, send its full text: `the sandboxed command (process N): did not exit
   within ...`, `... a process it started was still running ... after its tree was
   ended`, or `waited ... for the process spawn lock and gave up`. If a kgraf-host test
   stops making progress, its harness prints what it waits on (lock waits and running
   children, with their age); send that output too.

4. The worker's shell tool on Windows (a plain program and a `sh -c` line run and return
   their output, the network notice comes once under the token sandbox and never under the
   sandbox user, an exit status comes back, a timeout and a cancel end the whole process
   tree), and its pure decisions:

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
  dns: not confined (token sandbox; run `kgraf sandbox setup`)
  hidden folder: not writable
  tree: writable
```

Expected after `setup` (sandbox user). The direct connection to the local origin is blocked
(the loopback filters), the DNS filters refuse the sandbox user's lookup, and the exit code
is 0:

```
sandbox selfcheck: ok
  backend: Windows sandbox user
  network: only the egress proxy was reachable
  dns: confined (the sandbox user's lookup reached no DNS server, and the DNS Client service did not look the name up)
  hidden folder: not writable
  tree: writable
```

Both runs also print lines that start with `  launch:` on standard error (on the sandbox
user, the window stations kGraf made, the duration of each logon call, and the spawn
lock's use; see "The sandbox user phase" above). They are not part of the verdict; send
them with the rest.

Any other output is a failure. A failure starts with `sandbox selfcheck failed (backend: ...)`
and names the step. If only the `dns:` line failed, the first line is
`sandbox selfcheck: failed` and the `dns:` line says why: `not confined (lookups leave the
machine)` or `not proved`. Send back the full output of both runs.

Also check by hand after `setup`: `Get-LocalUser kgraf-*` lists two users plus two per
worker slot (ten with the default four slots: `kgraf-off-`, `kgraf-on-`, `kgraf-s00-` to
`kgraf-s03-` and `kgraf-o00-` to `kgraf-o03-`), they are not on the sign-in screen, and
`Get-NetFirewallRule -DisplayGroup "kGraf sandbox"` lists four block rules. The rules name
`kgraf-off-` and every `kgraf-sNN-` user, and no `kgraf-oNN-` user. Send back the output of
every command.

### The closed folders: the access lists before setup, after setup and after remove

The setup writes one deny entry for the sandbox group on each folder every local user may
write: the root of each fixed drive whose file system keeps access lists, `C:\ProgramData`,
`C:\Users\Public` and the folders directly in it, and `Temp`, `Tasks` and `tracing` under
the Windows folder. Those entries are on the real machine, so this run has to settle what
they change and what they leave alone.

Run this **three times**: before `kgraf sandbox setup`, after it, and after
`kgraf sandbox remove`. Save each run to its own file and send all three back.

```powershell
# Save as closed-acl.ps1 and run it as: .\closed-acl.ps1 before   (then: after, removed)
param([Parameter(Mandatory=$true)][string]$When)
$folders = @()
# The root of every fixed drive that keeps access lists.
Get-Volume | Where-Object { $_.DriveType -eq 'Fixed' -and $_.DriveLetter } |
  ForEach-Object { $folders += "$($_.DriveLetter):\" }
$folders += $env:ProgramData
$folders += $env:PUBLIC
Get-ChildItem -LiteralPath $env:PUBLIC -Directory -Force |
  ForEach-Object { $folders += $_.FullName }
foreach ($child in 'Temp','Tasks','tracing') { $folders += (Join-Path $env:SystemRoot $child) }

$out = "closed-acl-$When.txt"
"=== $When ===" | Out-File $out
foreach ($f in $folders) {
  "---- $f" | Out-File $out -Append
  if (-not (Test-Path -LiteralPath $f)) { "  (does not exist)" | Out-File $out -Append; continue }
  # 1. Every entry, in order, as icacls writes them.
  (icacls $f 2>&1) | Out-File $out -Append
  # 2. Whether the list is protected, and who owns it. The protected state is
  #    the one the setup must leave exactly as it found it.
  try {
    $acl = Get-Acl -LiteralPath $f -ErrorAction Stop
    "  AreAccessRulesProtected = $($acl.AreAccessRulesProtected)" | Out-File $out -Append
    "  Owner = $($acl.Owner)" | Out-File $out -Append
  } catch { "  Get-Acl failed: $_" | Out-File $out -Append }
}

# 3. Did any child of ProgramData or Public gain an entry? A write that
#    propagated would have rewritten every child's list. Counted, and the
#    first twenty listed with their entry counts, for comparison between the
#    three runs.
"---- children of ProgramData and Public" | Out-File $out -Append
foreach ($parent in $env:ProgramData, $env:PUBLIC) {
  Get-ChildItem -LiteralPath $parent -Force -ErrorAction SilentlyContinue |
    Select-Object -First 20 | ForEach-Object {
      try {
        $a = Get-Acl -LiteralPath $_.FullName -ErrorAction Stop
        "  $($_.FullName): rules=$($a.Access.Count) protected=$($a.AreAccessRulesProtected)" |
          Out-File $out -Append
      } catch { "  $($_.FullName): Get-Acl failed" | Out-File $out -Append }
    }
}

# 4. May Administrators rewrite the lists of the two Windows folders that are
#    not theirs? `Tasks` and `tracing` are the ones a setup can fail on, and
#    WRITE_DAC (`ChangePermissions`) is the right it needs.
"---- WRITE_DAC for Administrators" | Out-File $out -Append
foreach ($child in 'Tasks','tracing') {
  $f = Join-Path $env:SystemRoot $child
  if (-not (Test-Path -LiteralPath $f)) { "  $f (does not exist)" | Out-File $out -Append; continue }
  try {
    $a = Get-Acl -LiteralPath $f -ErrorAction Stop
    "  $f owner=$($a.Owner)" | Out-File $out -Append
    $a.Access | Where-Object { $_.IdentityReference -match 'Administrators' } |
      ForEach-Object {
        "    $($_.IdentityReference) $($_.AccessControlType) $($_.FileSystemRights)" |
          Out-File $out -Append
      }
  } catch { "  $f Get-Acl failed: $_" | Out-File $out -Append }
}
"wrote $out"
```

How to read the three files:

- **After setup**, each folder that exists must have gained exactly one deny entry, for a
  SID starting with `S-1-5-21-`, and it must carry no `(I)` inheritance mark. Nothing else
  on the folder may have changed: the entry order, every other entry, the owner, and
  `AreAccessRulesProtected` must read the same as before, folder by folder.
- **No child of ProgramData or Public** may have changed its rule count or its protected
  state between the runs. A change there means the write propagated, which writing the
  list with `SetKernelObjectSecurity`, on that folder alone, is there to prevent.
- **After remove**, every folder must read exactly as it did before setup, byte for byte in
  the `icacls` output.
- **A folder setup could not close** is named in the failure, with the principal that owns
  it. The `WRITE_DAC for Administrators` block says whether that was to be expected:
  Administrators need `ChangePermissions` on `Tasks` and `tracing`, and on some installs
  they do not have it. Send that block back whether the setup succeeded or not, because it
  settles whether those two folders can be closed on a real machine at all.
- `kgraf sandbox status` names every folder that is closed, so its output before, after and
  after remove is part of the same evidence.

### The DNS filter

`kgraf sandbox setup` adds three Windows Filtering Platform (WFP) filters that are meant
to make the DNS Client service refuse name lookups from the users the firewall rules block,
and four more at the connect layers that block those users' loopback connections outside
kGraf's egress proxy ports (one per IP version for every other port, one per IP version
for UDP); the four have their own status lines (`loopback filter on the connect layer,
IPv4 ...`), and selfcheck's `network:` line proves them. All seven name
`kgraf-off-` and every slot user without the network (`kgraf-sNN-`). They never name
`kgraf-on-`, a `kgraf-oNN-` user or the owner. Two sit on the name resolution cache layers
(IPv4 and IPv6), which see the local caller; the third sits on the user-mode RPC layer,
which sees a remote caller only. No Windows run has proved any of them yet, and it is not
known whether a block at the name resolution cache layers makes a lookup fail or only
keeps the service from its cache: that is what this run settles. These checks need an
administrator PowerShell and a working network. Run them after `setup`:

```powershell
# 1. The filter, its sublayer and its provider: every item whose name starts with "kGraf sandbox".
netsh wfp show filters file="$r\wfp-filters.xml"
Select-String -Path "$r\wfp-filters.xml" -Pattern "kGraf sandbox" -Context 3,40 | Out-File "$r\wfp-kgraf.txt"

# 2. The status line of the filter.
kgraf sandbox status | Out-File "$r\status-dns.txt"

# 3. The cache check: clear the cache, run selfcheck, and list the probe names in the cache.
Clear-DnsClientCache
kgraf sandbox selfcheck 2>&1 | Out-File "$r\selfcheck-dns.txt"
Get-DnsClientCache | Where-Object Entry -like "kgraf-probe-*" | Format-List | Out-File "$r\dns-cache.txt"

# 4. The DNS test: offline user, online user and owner.
cargo test -p kgraf-sandbox --test dns_windows -- --test-threads=1 --nocapture 2>&1 | Tee-Object "$r\dns-windows.txt"
```

What to expect:

1. `wfp-kgraf.txt` shows three filters, each with the action block, less any filter
   setup reported as refused (a `refused` line in its output, `FAILED` in status). One is on the layer
   `FWPM_LAYER_RPC_UM`, with two conditions: the RPC interface
   `45776b01-5956-4485-9f80-f428f7d60129` and the caller's token. One is on
   `FWPM_LAYER_NAME_RESOLUTION_CACHE_V4` and one on `FWPM_LAYER_NAME_RESOLUTION_CACHE_V6`,
   each with the single condition `FWPM_CONDITION_ALE_USER_ID`. It also shows the sublayer
   and the provider; the sublayer's weight is 65535, so no other sublayer is decided before
   it. Send back each entry whole: the condition the engine recorded, and
   whether any filter carries `FWPM_FILTER_FLAG_DISABLED`, decide what the next change is.
2. Three status lines start with `present  DNS filter on`, one per layer. A line that says
   `WRONG` with "the Base Filtering Engine has it disabled" means the filter exists and
   does not run. A line that says `FAILED` names a filter Windows refused at setup, with
   its error and the probe reading: DNS is not confined on that machine, and step 3 and
   the offline lookup of `dns_windows` are expected to fail there. The filter's netsh
   check in `dns_windows` does not look for it, and says so in a `measurement:` line.
3. `dns-cache.txt` lists one `kgraf-probe-...` name: the one your own process looked up.
   A second name means the DNS Client service looked the sandbox user's name up: the
   filter does not work. No name at all means the cache does not keep "does not exist"
   answers on this machine, and the check proves nothing there.
4. The three tests of `dns_windows` pass. One prints the status the offline user's
   lookups got (`offline user: DnsQuery status ..., getaddrinfo ...`). That status is how
   the filters' refusal looks to a program; send it back. Another prints each filter's
   entry from `netsh wfp show filters` and checks its name, layer and action.
5. **Restart the machine, then run steps 1 to 4 again, and send both sets back.** The
   documentation contradicts itself on whether a provider that names no service has its
   filters disabled at every start. One reboot settles which sentence is true. The case to
   look for is a filter that carries `FWPM_FILTER_FLAG_DISABLED` after the restart, or a
   `dns:` line that passed before it and does not pass after it. A status line that says
   `WRONG ... disabled`, or the `dns_windows` test that fails on the disabled flag, is
   that case.

After `kgraf sandbox remove`, step 1 shows no item named `kGraf sandbox`, and the status
lines of the filters are `MISSING`.

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
   fails to connect. A lookup of a name must fail too (the DNS filters); the checks under
   "The DNS filter" above prove whether it left the machine.
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
