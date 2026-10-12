# Testing kGraf on Windows

## 1. Purpose and reader

This guide tests kGraf's Windows sandbox and the worker's shell on Windows, and says what to
send back. It is for a person with a Windows machine who can use PowerShell and run a
downloaded program. The bundle path needs no Rust knowledge. The manual path needs a Rust
build. Decisions 0011 and 0097 give the design; 0097 replaces the restricted-token design of
0011 with dedicated sandbox users.

Seven terms recur:

- The **token sandbox** is the backend kGraf uses when no sandbox user exists. A command runs
  with a restricted copy of the current user's own Windows token. It needs no administrator rights.
  It cannot deny network access, and Git Bash and some build tools fail under it.
- A **sandbox user** is a dedicated local Windows account that `kgraf sandbox setup` creates
  once, with administrator rights. A sandboxed command runs as that account. Setup makes two
  accounts per worker **slot** (one with network access, one without), so that two worktrees
  run as two different users.
- The **baseline** is the first full pass of the test suite, with no sandbox user set up.
- A **Windows Filtering Platform (WFP) filter** is a rule in the Windows network stack.
  Setup adds seven: four that block loopback connections (TCP outside kGraf's proxy ports,
  and UDP), and three that are meant to block name lookups (DNS). kGraf's **egress proxy** is the local
  proxy that is the only network path a sandboxed command without network access may use.
- The **launcher service** is a Windows service that setup installs. It starts each
  sandboxed command in session 0 (the non-interactive Windows session), where cargo and
  rustc can create the named objects (semaphores, mutexes) they need.

Latest recorded result (2026-10-11, section 4):

- The self-test bundle has run on a GitHub-hosted runner: Windows Server 2025 on x86_64, with
  Git for Windows, as an administrator. Its last full run read clean except for one named
  gap: DNS is not confined on Windows Server 2025 (section 5).
- Nothing has run on a standard-user Windows 11 machine, on ARM64, on a Windows version
  other than Server 2025, or on a machine without Git for Windows. The ARM64 notes in
  section 6 are written but untested.
- The bundle is the first choice. It takes about two hours on the hosted runner, and less
  on a local machine without the sandbox user phase.

Run every command in **Windows PowerShell or PowerShell 7, as a normal user** (not "Run as
administrator"), except where a step says it needs administrator rights. The sandbox is made
for a user without administrator rights, and an elevated shell tests something else.

## 2. The bundle: build, run, read the report

`kgraf-wintest.exe` is one program, shipped in a zip with kGraf's test binaries built for
Windows. It needs Git for Windows and nothing else: no Rust, no build tools, no API key. It
installs nothing.

1. Build the zip on macOS or Linux with `scripts/package-wintest.sh`. It writes
   `dist/wintest/kgraf-wintest-<version>-<commit>-windows-x86_64.zip`. On Windows, the
   GitHub workflow (section 3) builds the same zip natively.
2. On the Windows machine, unblock the zip (Properties, "Unblock"). Unzip it to a folder
   such as `C:\kgraf-wintest`, not to Downloads or the Desktop (the sandbox users must be
   able to read it). Double-click `kgraf-wintest.exe`. Answer whether to run the extended
   set, or wait 20 seconds for yes.
3. Send back the one report it writes to the Desktop,
   `kgraf-wintest-report-<date>-<time>.md`. Delete the unzipped folder.

What it runs, in order:

1. The machine facts: Windows build and edition, architecture, processor, memory, Git for
   Windows and Git Bash, winget, Defender, SmartScreen, locale.
2. The core set: every `kgraf-sandbox` test binary, the `windows` tests of `kgraf-tools`,
   and the network notice, dependency link and check input tests of `kgraf-host` and the
   orchestrator.
3. End-to-end checks with the real `kgraf.exe`, with a scripted model on 127.0.0.1 in place
   of a provider.
4. If chosen, the extended set: every other test binary that runs without Rust, a model,
   Docker or the internet.

Each test binary runs with `--test-threads=1`. The test binaries were built on another
machine, so the runner copies the sources into a work folder at the build machine's relative
path and maps a free drive letter to it with `subst`. Tests that build with cargo at run
time, need a model key or Docker, or read git history are left out. The manifest in the zip
lists each with its reason.

The report has a summary table, then every failure with its output, the counts and times per
binary, what was not run and why, and the same data as JSON. It replaces the user name and
computer name and records no environment variables. When a binary hangs or crashes, the
runner runs each failed test again alone and puts that output in the report, marked as a solo
re-run. A solo re-run never changes the first run's result.

The runner leaves only the report. Every other file is in one work folder under `%TEMP%`,
which it removes at the end, also after Ctrl+C. It then checks that the drive letter and the
folder are gone and that no kGraf folder appeared under `%APPDATA%` or `%LOCALAPPDATA%`.

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

The bundle is built for x86_64 only. On ARM64 it runs under emulation and the report says so.

**What stays manual:** installing the prerequisites and building from source (section 6);
the Git for Windows install offer (case 6a); the dependency install (case 7); the desktop
window and the installer; and the Windows Firewall test in `MANUAL-NETWORK-TEST.txt`. For
manual tests, `kgraf-wintest.exe sandbox-run -- <program> [args...]` runs one program inside
kGraf's sandbox in a temporary folder and prints its exit code.

### The sandbox user phase

The flag `--sandbox-user` adds a phase after the baseline. It needs administrator rights:
start the runner from an elevated prompt or accept the UAC prompt. The hosted runner is
already elevated.

| Step | Command | What it checks |
|---|---|---|
| Before setup | `kgraf sandbox selfcheck` | For comparison. On the token sandbox it passes with a gap: the `network:` and `dns:` lines read "not confined". |
| Setup | `kgraf sandbox setup --slots 4` | Creates the sandbox users, the group, the firewall rules and the seven WFP filters. A filter Windows refuses does not stop setup. A `refused` line names it, and a closing notice says DNS, loopback connections or both are not confined on this machine. |
| Status | `kgraf sandbox status` | Every kGraf item is present, and every stored password logs on. A refused filter shows as `FAILED` with the reason. |
| Selfcheck | `kgraf sandbox selfcheck` | The check that decides the phase. It must exit 0, print `sandbox selfcheck: ok` and name the backend `Windows sandbox user`. |
| Re-run | the test binaries again | See below. Runs with `KGRAF_REQUIRE_SANDBOX_USER=1`. |
| Remove | `kgraf sandbox remove`, then `kgraf sandbox status` | Always runs, also after a failure or Ctrl+C. Every kGraf item must read `MISSING`, the launcher service included. |

If selfcheck passes on the token sandbox, setup did not take effect, and the phase stops. If
it fails on its `dns:` line alone, the phase goes on, and the summary says `DNS FAILED`. Any
other failure stops the phase.

If the Windows user who runs the phase already has a sandbox user, the phase does not start,
because its last step would remove it. Run `kgraf sandbox remove` first.

The re-run runs the sandbox test binaries whole. Of every other baseline binary whose
failures name a token sandbox limit, it runs only the tests that failed. It also re-runs the
end-to-end checks. With `KGRAF_REQUIRE_SANDBOX_USER=1`, a test that would take the token
sandbox branch, or skip, fails instead. Without the switch, a machine where setup did not take
effect passes every test and measures nothing. The runner sets the switch only for the re-run.

**How the verdict counts failures.** The verdict line counts four things: every baseline
failure not classed as expected; every failed test and every hung or crashed binary of the
re-run; every failed end-to-end check; and any item the time budget left unrun.

A baseline failure is **expected under the token sandbox** only when both hold: the same test
passed in the re-run, and its own baseline output names a known token limit. The known
limits are the cargo jobserver (cargo's shared build-slot semaphore), Git Bash (msys),
`NtCreateDirectoryObject`, the signal pipe, Schannel (Windows' own TLS library), and
`STATUS_DLL_INIT_FAILED` (0xC0000142). Expected failures are not in the
verdict. A section "Expected under the token sandbox (N)" lists each with its binary, its
test and its marker. A failure without a marker, or one whose re-run failed, hung or did not
run, still counts. An end-to-end check follows the same rule: `shell.git_bash` is the known
case.

When selfcheck failed on `dns:`, a line `Known gap: DNS is not confined on this machine ...`
follows the verdict. The DNS test of the re-run
(`kgraf-sandbox/test/dns_windows :: an_offline_lookup_does_not_reach_the_dns_client_service`)
then fails. It is listed under "Known gaps" and not counted, only when selfcheck failed on DNS
alone and the test's own output says "This is the recorded gap". A leak with every filter in
place prints "a defect of the filters", and counts.

### The time budget

A host that kills a job at a fixed limit leaves no report. `kgraf-wintest.exe --budget-mins
<N>` gives the run N minutes, counted from the runner's start, for everything including the
removal and the report. The runner does not start an item whose expected time exceeds the
time left, and it cuts a running item's limit to the time left. It keeps a reserve at the end
(540 s with the sandbox user phase) for removal, cleanup and the report. The verdict reads
`INCOMPLETE` as soon as one item did not run or was stopped, and the runner exits 1.

The workflow sets N to the job limit (165 minutes), less the minutes already used, less 12
for the steps after the bundle, less 1.

### Running only some tests

`kgraf-wintest.exe --only <filter>` runs only what the filter names, in the baseline and in
the re-run. The filter is a comma-separated list:

- `<run id>`: one test binary, whole. Run ids are in the report's "Test binaries" table, for
  example `kgraf-sandbox/test/escape_windows`.
- `<run id>::<test name>`: one test, by the full name the report prints.
- `e2e`: every end-to-end check. `e2e::<check id>`, for example `e2e::shell.git_bash`,
  keeps one.

An item that names nothing in the bundle stops the runner before it starts. With a filter,
the runner asks nothing about the extended set. The report states the filter above the
verdict.

## 3. Workflow inputs

The `wintest.yml` workflow builds the bundle natively on a GitHub-hosted Windows runner,
runs `kgraf-wintest.exe --all --no-wait`, runs the Windows Firewall test
(`scripts/wintest-firewall.ps1`) and uploads one artifact. It has a manual trigger only:

```
gh workflow run wintest.yml
gh run watch
gh run download <run id> -n kgraf-windows-selftest
```

The artifact holds `report\kgraf-wintest-report-*.md`, `firewall.txt` and, when the probe
ran, `appcontainer-dns.md` with raw captures. The upload runs after a cancel or a timeout, so
a partial result survives. A failing test does not fail the job. A failed build or a missing
report file does.

The workflow has four inputs, set with `-f name=value`:

| Input | Type, default | Meaning |
|---|---|---|
| `sandbox_user` | boolean, `true` | Adds `--sandbox-user`: after the baseline, set up the sandbox users, check them, re-run the tests as them, and remove them. Set `false` for the baseline alone. |
| `only` | string, empty | Passed to the runner as `--only <value>`. Empty runs everything. |
| `appcontainer_dns` | boolean, `true` | After the bundle, runs `scripts/wintest-appcontainer-dns.ps1`, a probe of whether a Windows AppContainer (a low-privilege process container) confines DNS. It never fails the job. |
| `probe_only` | boolean, `false` | Runs only that probe and the upload. It checks out the `scripts` folder, installs no Rust and builds nothing. The bundle, the tests, the sandbox user phase and the firewall test are skipped. |

Examples:

```
gh workflow run wintest.yml -f sandbox_user=false
gh workflow run wintest.yml -f only='kgraf-sandbox/test/escape_windows'
gh workflow run wintest.yml -f probe_only=true
```

Before the bundle runs, the workflow does three things. Each fails the job if it does not
work:

- It copies the bundle to `C:\kgraf-wintest` with a protected access list (full control for
  SYSTEM and Administrators, read and execute for Users). The sandbox users cannot read the
  runner account's profile, and the checkout's access list comes from the image.
- It starts the OpenSSH agent service, which is off on the hosted image. An escape test
  probes its pipe.
- It checks that Git for Windows is installed.

Two things differ from a standard user's machine. The runner account is an administrator, and
the runner has Git for Windows. A hosted run does not replace a run on a standard-user
machine without Git for Windows.

**Cost.** GitHub-hosted standard runners are free on a public repository.

## 4. What a clean run looks like

A **confirmation run** is a full run (no `only`) with the sandbox user phase. It reads clean
when all six hold:

- The verdict line reads `ALL PASSED`, with no failed test, no hung or crashed binary and no
  failed end-to-end check. While the DNS gap stands, a `Known gap: DNS ...` line follows it.
- "Expected under the token sandbox (N)" lists the baseline's Git Bash, jobserver and
  0xC0000142 failures and the end-to-end check `shell.git_bash`, and each one passed as the
  sandbox user.
- The time budget was not reached: no item under "Not run" says `not run: budget`.
- Selfcheck prints `launch:` lines that show the launcher service made a window station
  `kgraf-cmd-...` for each command in session 0, and `network: only the egress proxy was
  reachable`.
- In the re-run, every test that failed in the baseline for a token sandbox limit passes,
  and no binary timed out. These include the `escape_windows` tests of the command's desktop,
  `runner_windows`, `startup_windows`, and the failed tests of `kgraf_host`,
  `kgraf_bench_briefs` and `contract_checks`.
- Removal leaves nothing: "Launcher service after the removal: gone", "Fallback grant after
  the removal: gone", and "Confirmed: nothing of the sandbox user is left".

A verdict that counts any other test, a hung binary, an end-to-end failure, a
`not run: budget` item or a non-empty status after removal is not clean.

### Latest recorded result

On 2026-10-11 a full run with the sandbox user phase read clean on a hosted runner (Windows
Server 2025 Datacenter 24H2, build 26100.33438, x86_64, 2 cores, 8 GiB, Git for Windows
2.55.0, cargo 1.99.0). The verdict was `ALL PASSED`, with DNS as the only known gap.

- 181 test binaries ran; none hung or crashed. 5,577 tests passed, 143 failed in the baseline
  and 11 were ignored.
- The 144 baseline failures (143 tests and `shell.git_bash`) were all expected under the token
  sandbox, and every one passed as the sandbox user (77 `STATUS_DLL_INIT_FAILED`, 34
  jobserver, 32 Git Bash, 1 Schannel).
- The re-run took 15 binaries. All passed except `dns_windows` (3 passed, 1 failed, the known
  gap).
- The bundle took 1:36:25, of which 3,022 s went to the sandbox user phase. The job took
  119.7 minutes. The budget of 131 minutes was not reached.
- The launcher service started every command in session 0. The fallback grant did not run.

## 5. Known gaps

**DNS is not confined on Windows Server 2025.** Of the three DNS filters setup adds, the one
on the user-mode RPC layer is accepted. The two on the name resolution cache layers (IPv4
and IPv6) are refused with `FWP_E_OUT_OF_BOUNDS`. The offline sandbox user's lookup then
reaches a DNS server and gets an answer (status 9003, `DNS_ERROR_RCODE_NAME_ERROR`). So:

- `kgraf sandbox selfcheck` fails on its `dns:` line alone. Every other line passes,
  including the loopback filters and the egress proxy line.
- `dns_windows :: an_offline_lookup_does_not_reach_the_dns_client_service` fails with "This
  is the recorded gap".
- Every recorded run that wrote a report shows the same gap.

Another Windows version may accept those filters. None was tested. A run on one is the most
useful new result.

**The AppContainer probe.** A probe on 2026-10-11 found that the DNS Client service refuses a
lookup from an AppContainer that has no network capability. Git and `cargo build` did not run
inside the container, so decision 0097 keeps the sandbox user and records the DNS gap.

**Not tested:**

- A standard-user Windows 11 machine, ARM64, and a machine without Git for Windows.
- A Windows version other than Server 2025.
- The fallback grant on a real run. It exists for the case where the session-0 launcher
  cannot make named objects. The launcher path proved in every full run, so the fallback
  never ran.
- A test that passed in the baseline, run again as the sandbox user, outside the sandbox
  test binaries. The re-run of any other binary covers only its baseline failures.

## 6. Manual fallback

Use these steps to test a native build, to test ARM64, or when the bundle cannot run. A first
build takes 10 to 30 minutes.

**Machine.** `$env:PROCESSOR_ARCHITECTURE` prints `AMD64` (x86_64) or `ARM64`.

**Prerequisites.** Each `winget` command installs one program. Close and reopen PowerShell
after the last.

```powershell
winget install --id Git.Git -e --source winget
winget install --id Microsoft.VisualStudio.2022.BuildTools --source winget --force --override "--wait --passive --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --add Microsoft.VisualStudio.Component.VC.Llvm.Clang --add Microsoft.VisualStudio.Component.VC.Llvm.ClangToolset --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --addProductLang En-us"
winget install --id NASM.NASM -e --source winget
winget install --id Rustlang.Rustup -e --source winget
```

- Git for Windows provides Git Bash, which kGraf uses for `sh` and `bash` commands.
- The Build Tools command installs the C++ linker and the Windows SDK. The two `Llvm.Clang`
  components are for ARM64, where `aws-lc-sys` (a TLS library) compiles C with `clang-cl`. If
  winget reports a newer Visual Studio, run `winget search Microsoft.VisualStudio` and use the
  `BuildTools` id it lists.
- NASM is for x86_64 only. If it cannot be installed, set
  `$env:AWS_LC_SYS_PREBUILT_NASM = "1"` before building.

Check: `rustup show` names `stable-x86_64-pc-windows-msvc` (or `stable-aarch64-pc-windows-msvc`
on ARM64); `cargo --version` is 1.95 or later (else `rustup update`);
`& "$env:ProgramFiles\Git\bin\bash.exe" -c "uname -a"` prints a line starting with
`MINGW64_NT` or `CLANGARM64_NT`.

**Build.** Use a folder on an NTFS drive: the sandbox works through file permissions and
refuses FAT and exFAT.

```powershell
mkdir C:\src -Force; cd C:\src
git clone https://github.com/Marceux-Croix/kGraf.git
cd kGraf
cargo build -p kgraf-cli
mkdir C:\src\kgraf-results -Force
$r = "C:\src\kgraf-results"
```

On ARM64 the native target is `aarch64-pc-windows-msvc`. To test the emulated x86_64 build
too, run `rustup target add x86_64-pc-windows-msvc`, build with
`--target x86_64-pc-windows-msvc`, and repeat each `cargo test` command below with that
option. The ARM64 path has not been run.

**Tests.** Run each command to the end, even when an earlier one failed. None is marked
`#[ignore]`. A passing binary ends with `test result: ok.`; write down every `FAILED` line.
Without a sandbox user, some Git Bash and cargo tests fail with status `-1073741502`
(0xC0000142), `couldn't create signal pipe` or `failed to create jobserver`. These are the
token sandbox limits listed in section 2. They pass as the sandbox user, so run steps 2 to 3c
again after `kgraf sandbox setup` (case 6b) with `$env:KGRAF_REQUIRE_SANDBOX_USER = "1"`.

| Step | Command | Pass line |
|---|---|---|
| 1. Sandbox unit tests | `cargo test -p kgraf-sandbox --lib -- --nocapture 2>&1 \| Tee-Object "$r\1-sandbox-lib.txt"` | `test result: ok.`; send every line containing `MEASURE` |
| 2. `sh` and `bash` in the sandbox | `cargo test -p kgraf-sandbox --test shell_windows -- --test-threads=1 2>&1 \| Tee-Object "$r\2-shell.txt"` | ok as sandbox user |
| 2a. Programs that load `user32.dll`, `cmd`, a cleared environment | `cargo test -p kgraf-sandbox --test startup_windows -- --test-threads=1 2>&1 \| Tee-Object "$r\2a-startup.txt"` | ok as sandbox user |
| 3. Escape tests | `cargo test -p kgraf-sandbox --test escape_windows -- --test-threads=1 --show-output 2>&1 \| Tee-Object "$r\3-escape.txt"` | ok (45 tests on the last run); send every line starting with `measurement:` |
| 3a. HTTPS in the sandbox, listener on 127.0.0.1 | `cargo test -p kgraf-sandbox --test tls_windows 2>&1 \| Tee-Object "$r\3a-tls.txt"` | ok; System32 `curl.exe` fails inside the sandbox with `SEC_E_NO_CREDENTIALS`, and the test expects that |
| 3b. What a command can do to its runner | `cargo test -p kgraf-sandbox --test runner_windows -- --test-threads=1 --nocapture 2>&1 \| Tee-Object "$r\3b-runner.txt"` | ok (11 tests on the last run), no "skipped" line; send the `launch:` lines |
| 3c. Standard handles | `cargo test -p kgraf-sandbox --test stdio_windows -- --test-threads=1 --nocapture 2>&1 \| Tee-Object "$r\3c-stdio.txt"` | ok (8 tests on the last run); send the `launch:` lines |
| 4. Worker shell tool | `cargo test -p kgraf-tools --lib windows -- --test-threads=1 2>&1 \| Tee-Object "$r\4-tools.txt"` | ok |
| 5. Run-start notices | `cargo test -p kgraf-host --lib network_notice 2>&1 \| Tee-Object "$r\5-notices.txt"` | ok |
| 6. Optional: all four packages | `cargo test --no-fail-fast -p kgraf-sandbox -p kgraf-tools -p kgraf-host -p kgraf-bench-briefs -- --test-threads=1 2>&1 \| Tee-Object "$r\6-four.txt"` | a failure not in steps 1 to 5 is worth reporting |

Notes for steps 3 to 3c:

- The escape tests use the test binary itself as the probe, and a sandbox user cannot read
  the current user's profile. Set `$env:CARGO_TARGET_DIR = "C:\kgraf-target"` (outside the profile)
  before building and running them. A test fails with a message that names
  `CARGO_TARGET_DIR` if the binary is under the profile.
- Many escape tests report a measurement and not only a block, such as whether a sandbox
  user can write an Everyone-writable folder under ProgramData, read a file there, or open
  the SSH agent's pipe. libtest prints a passing test's output only with `--show-output`, so
  add it and copy every `measurement:` line.
- With `KGRAF_REQUIRE_SANDBOX_USER=1`, a test fails where a thing it probes is absent, and
  does not skip. Start the OpenSSH agent (`Start-Service ssh-agent`), run from a PowerShell
  window (the test needs a console), have Git for Windows installed, and set up at least two
  slots (`kgraf sandbox setup --slots 2`).
- Run step 3c once before `kgraf sandbox setup` and once after.
- If a wait ends with `the sandboxed command (process N): did not exit within ...` or
  `waited ... for the process spawn lock and gave up`, send the full text.

## 7. Cases that need the real app

Cases 5 and 6 need one model route: a hosted provider with its key in the window (for
example `$env:KGRAF_OPENAI_API_KEY = "<key>"` with `--provider openai`) or a local server
such as Ollama (`--provider ollama`). Cases 6a, 6b and 7 do not.

### Case 5: a short run that uses the worker shell

```powershell
cd C:\src; cargo new kgraf-smoke; cd kgraf-smoke
git add -A
git -c user.name=smoke -c user.email=smoke@example.invalid commit -m "start"
$kgraf = "C:\src\kGraf\target\debug\kgraf.exe"
& $kgraf index
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

Answer "allow" to each command. Pass: command 2 prints `hello from git bash`, a line
starting with `MINGW64_NT` or `CLANGARM64_NT`, and `a b|c|`; command 3 prints nothing or a
short status; command 4 builds and runs zero tests with `test result: ok`. Without a sandbox
user, the first result ends with a `[sandbox] network not blocked` notice, once. With one,
the notice does not appear. If Git Bash is missing, command 2 fails with a message naming Git
for Windows and `KGRAF_GIT_BASH_PATH`; report it only if Git for Windows is installed.

### Case 6: cmd and PowerShell scripts under a rule

This case checks that kGraf asks about a command it cannot read as plain text, and runs a
plain one on its grants. Write a permissions file for this run only, then run six commands:

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

Answer "deny" to every question. Pass: commands 1 to 3 run with no question. Commands 4 to 6
each ask, because each holds a quote character that kGraf cannot check against the rule
`shell(curl*)`. If the model changes an argv, note which commands it changed.

### Case 6a: the Git for Windows check and install offer

Use a fresh Windows 11 machine or virtual machine with the built `.msi` or `.zip`, as a normal
user. `winget` needs Windows 10 1809 or later with App Installer. Write what each case
showed into `$r\6a-git-check.txt`.

1. **No Git, winget present.** Confirm `git --version` and `reg query
   HKLM\SOFTWARE\GitForWindows` both fail. Start `kgraf-app.exe`. Pass: one window offers to
   install Git for Windows with winget and shows the full `winget install --id Git.Git ...
   --scope user ...` line. Answer Yes. No administrator prompt appears, and kGraf opens
   without a restart.
2. **Same, answer No.** First `winget uninstall --id Git.Git`. Pass: an error window gives
   the download link, the winget command and `KGRAF_GIT_BASH_PATH`, and kGraf exits.
3. **Same, command line.** Reset. Run `kgraf.exe index`. Pass: the same question with
   `[y/N]`. With input redirected (`echo | kgraf.exe index`) it must not ask, and must stop
   with the same fix text.
4. **With Git.** Install Git by hand. Pass: no question and no delay. Repeat for an
   all-users install (`HKLM`), a per-user install (`HKCU`), and an install on another drive
   that is not on `PATH`.
5. **No winget.** Rename `$env:LOCALAPPDATA\Microsoft\WindowsApps\winget.exe` to `winget.off`,
   with Git not installed. Pass: an error window says winget is not available and gives the
   link and `KGRAF_GIT_BASH_PATH`, with no question. Rename it back.
6. **Bad override.** Set `KGRAF_GIT_BASH_PATH` to a file that does not exist. Pass: an error
   that names the variable and the file, and no offer to install.

### Case 6b: the sandbox user setup

Run in a normal PowerShell. Setup and remove ask for administrator rights through a User Account Control (UAC)
prompt.

```
kgraf sandbox status      # before: users and rules MISSING
kgraf sandbox selfcheck   # before setup
kgraf sandbox setup       # click Yes; each step prints its outcome
kgraf sandbox status      # after: every line present
kgraf sandbox selfcheck   # after setup
kgraf sandbox setup       # again: passwords reset, rules refreshed
kgraf sandbox remove      # every item removed
kgraf sandbox status      # everything MISSING again
```

Before `setup` (token sandbox), the exit code is 0 and the output is:

```
sandbox selfcheck: passed with a gap
  backend: Windows restricted token (token sandbox)
  network: not confined (token sandbox; run `kgraf sandbox setup`)
  dns: not confined (token sandbox; run `kgraf sandbox setup`)
  hidden folder: not writable
  tree: writable
```

After `setup` (sandbox user), the result depends on the machine, so there are two outcomes
for the `dns:` line:

- **A machine whose WFP accepts all three DNS filters:**

  ```
  sandbox selfcheck: ok
    backend: Windows sandbox user
    network: only the egress proxy was reachable
    dns: confined (the sandbox user's lookup reached no DNS server, and the DNS Client service did not look the name up)
    hidden folder: not writable
    tree: writable
  ```

  "Confined" is correct only where a filter layer exists that blocks the lookup. Every
  machine tested so far was Windows Server 2025, which does not give this result.
- **Windows Server 2025** refuses the name resolution cache filters at setup (`refused` line,
  `FAILED` in status). Selfcheck then prints `sandbox selfcheck: failed`, every line passes
  except `dns:`, and that line reads `not confined (lookups leave the machine)` or
  `not proved`. Setup exits 0 and says DNS is not confined on this machine. This is the
  known gap of section 5, and it is the result to expect there.

Any other output is a failure. It starts with `sandbox selfcheck failed (backend: ...)` and
names the step. Both runs also print `  launch:` lines on standard error. Send them with the
rest. Send the full output of both runs.

After `setup`, check by hand:

1. `Get-LocalUser kgraf-*` lists two users plus two per slot (ten with four slots:
   `kgraf-off-`, `kgraf-on-`, `kgraf-s00-` to `kgraf-s03-`, `kgraf-o00-` to `kgraf-o03-`).
   They are not on the sign-in screen. `Get-NetFirewallRule -DisplayGroup "kGraf sandbox"`
   lists four block rules.
2. In a sandboxed shell, `whoami` prints `kgraf-sNN-<hash>` (a command that needs the network
   prints `kgraf-oNN-<hash>`). `dir C:\Users\<your name>` is denied.
3. `C:\Windows\System32\curl.exe -sS -o NUL -w "%{http_code}" https://example.com` prints
   `200` for a command with network access and fails to connect for one without.
4. In a worktree (a separate checked-out copy of a repository), a sandboxed command can write inside the worktree and cannot write in a
   sibling folder, a top-level `.git` entry or a hidden folder. Two worktrees with a
   30 second command each print two different `kgraf-sNN-` users.
5. After `kgraf sandbox remove`, `icacls <a worktree>` shows no `kgraf-` entry.

Setup also writes one deny entry for the sandbox group on each folder that every local user
may write (the root of each fixed drive, `C:\ProgramData`, `C:\Users\Public` and the folders
directly in it, and `Temp`, `Tasks` and `tracing` under the Windows folder). `kgraf sandbox
status` names every closed folder. Send its output before setup, after setup and after
remove; after remove, each folder's `icacls` output must read as it did before setup. If
setup cannot close a folder, the failure names it and the account that owns it.

### Case 7: a dependency install in the sandbox

This case needs Node.js with npm (`winget install OpenJS.NodeJS.LTS`) and the network. It
checks that kGraf runs a project's install in its sandbox, that the install script cannot
write outside the project or into `.kgraf`, and that kGraf asks for consent.

```powershell
cd C:\src; mkdir kgraf-install-check; cd kgraf-install-check
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
"install-links=true" | Set-Content .npmrc
& $kgraf deps status 2>&1 | Tee-Object "$r\9-deps-status.txt"
& $kgraf deps install npm 2>&1 | Tee-Object "$r\10-deps-install.txt"
Get-Content node_modules\probe\probe-result.json | Tee-Object "$r\11-probe-result.txt"
Test-Path C:\src\kgraf-escape.txt | Tee-Object -Append "$r\11-probe-result.txt"
Test-Path .kgraf\planted.txt | Tee-Object -Append "$r\11-probe-result.txt"
```

Pass:

- `deps status` lists `install: npm install in the project root (npm)` with the stored answer
  `unset`.
- `deps install npm` prints `installed  ran `npm install` in the project root in kGraf's
  sandbox`. Without a sandbox user it adds that the network was open and reads were not
  confined. With one, the install goes through kGraf's egress proxy and the notice is absent.
- `node_modules\left-pad` exists. `probe-result.json` shows `outside` and `state` as `denied`
  with `EPERM` or `EACCES`. Both `Test-Path` lines print `False`. A `written` or a `True` is a
  finding: send it.
- After deleting `node_modules`, `& $kgraf deps allow npm` prints `npm: allowed `npm
  install` in the project root`, and `& $kgraf deps reset npm` forgets it.

## 8. What to send back

For the bundle, send the one report file. For the manual path, collect the machine facts and
zip the results folder:

```powershell
rustc -vV | Out-File "$r\versions.txt"
cargo --version | Out-File -Append "$r\versions.txt"
git --version | Out-File -Append "$r\versions.txt"
& "$env:ProgramFiles\Git\bin\bash.exe" --version | Select-Object -First 1 | Out-File -Append "$r\versions.txt"
$env:PROCESSOR_ARCHITECTURE | Out-File -Append "$r\versions.txt"
Get-ComputerInfo -Property OsName, OsVersion, OsBuildNumber, OsArchitecture | Out-File -Append "$r\versions.txt"
Compress-Archive -Path "$r\*" -DestinationPath C:\src\kgraf-windows-results.zip -Force
```

Send `C:\src\kgraf-windows-results.zip`. Say which machine it came from: Windows edition and
build, x86_64 or ARM64 (and on ARM64, native or emulated build), whether it is a standard-user
or administrator account, and whether Git for Windows was installed before the run. Look
inside the files first. They hold the commands' output and paths on your machine, and the
events file holds what the model wrote. No API key is in them.

For any failure, the most useful evidence is the failing test's full output, the `launch:`
and `measurement:` lines, and the output of `kgraf sandbox status`.
