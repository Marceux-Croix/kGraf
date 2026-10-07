# Testing kGraf on Windows

The Windows sandbox (decision 0011) and the worker's shell on Windows are built and
compile-checked from macOS, but nothing in them has run on a Windows machine yet. These
steps test them on a fresh Windows 11 machine, on x86_64 (Intel or AMD) and on ARM64
(Snapdragon). They take about an hour, most of it the first build.

Run every command in **Windows PowerShell or PowerShell 7, as a normal user** (not "Run as
administrator"): the sandbox is made for a user without administrator rights, and an
elevated shell would test something else.

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
   decision):

   ```powershell
   cargo test -p kgraf-sandbox --lib 2>&1 | Tee-Object "$r\1-sandbox-lib.txt"
   ```

2. `sh` and `bash` through Git Bash inside the sandbox:

   ```powershell
   cargo test -p kgraf-sandbox --test shell_windows -- --test-threads=1 2>&1 | Tee-Object "$r\2-shell-windows.txt"
   ```

3. The sandbox escape tests (writes outside the worktree, the user profile, another
   sandbox's process, the registry, a junction, and a real cargo build inside the sandbox):

   ```powershell
   cargo test -p kgraf-sandbox --test escape_windows -- --test-threads=1 2>&1 | Tee-Object "$r\3-escape-windows.txt"
   ```

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
  run with the network open`. Later results do not repeat it.
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

## 7. What to send back

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
