kGraf Windows self-test
=======================

What this does: runs kGraf's Windows tests on this machine and writes one
report file to your Desktop. It needs no Rust and installs nothing.

Steps
-----
1. Before you unzip: right-click the zip, Properties, tick "Unblock" if it is
   there, OK. (This stops Windows asking about each file.)
2. Unzip the whole zip to a folder (for example your Downloads folder).
3. Open the unzipped folder and double-click kgraf-wintest.exe.
   If Windows shows "Windows protected your PC", click "More info", then
   "Run anyway". The program is not signed.
   Do not use "Run as administrator". Nothing in the run needs it.
4. Answer the one question at the start (or wait 20 seconds for "yes"):
   it asks whether to run the extended set of tests as well.
5. Wait. The bar shows the progress and an estimate of the time left.
   Ctrl+C stops the run; it still cleans up and writes a partial report.
6. At the end it prints the report path and waits for Enter. Send that one
   file back: kgraf-wintest-report-<date>-<time>.md on your Desktop.
7. Delete the unzipped folder. (At the end the program offers to do this
   for you; say no if you still want to run MANUAL-NETWORK-TEST.txt.)

What is left on the machine afterward: the report file only. Everything
else the run creates goes into one work folder under %TEMP%, which is
removed at the end and checked. The report lists what was created and
removed.

Needs: Windows 10 or 11 on an NTFS drive, and Git for Windows
(https://gitforwindows.org/). Without Git for Windows many tests fail; the
report says so. The program never installs Git or anything else.

What it runs
------------
- Machine facts: Windows version and build, edition, processor and
  architecture (and whether x86_64 programs run emulated on ARM64), memory,
  Git for Windows and Git Bash, winget, Defender and SmartScreen state,
  locale. No environment variables and no secrets are recorded; your user
  name and computer name are replaced in the report.
- The core set: kGraf's Windows-specific tests (the sandbox, its escape
  tests, sh and bash through Git Bash, the worker shell, the cmd and
  PowerShell readers, the network notice, dependency junctions).
- End-to-end checks with the real kgraf.exe: a worker run that uses the
  shell through Git Bash with a scripted model on 127.0.0.1 (no API key,
  no internet), the cmd and PowerShell reader decisions, the network
  notice, and the Git for Windows check. Cases that need Git to be missing
  are simulated and marked SIMULATED in the report.
- The extended set (optional): every other kGraf test that runs without
  Rust, a model provider, Docker or the internet.

For manual tests
----------------
  kgraf-wintest.exe sandbox-run -- <program> [args...]
runs one program inside kGraf's sandbox (network allowed) in a temporary
folder, prints its output and exit code, and removes the folder. See
MANUAL-NETWORK-TEST.txt for the one manual test that uses it.

Sandbox user phase (optional, needs administrator rights)
---------------------------------------------------------
  kgraf-wintest.exe --all --sandbox-user
runs the suite as usual, then sets kGraf's Windows sandbox user up, re-runs
the parts it changes, and removes it again. Start it from an elevated prompt,
or accept the UAC prompt. It does not start if this Windows user already has
a sandbox user set up. The report shows both results side by side.

Other options: kgraf-wintest.exe --help

Folder layout
-------------
  kgraf-wintest.exe        the runner
  README.txt               this file
  MANUAL-NETWORK-TEST.txt  a manual Windows Firewall test (administrator)
  bundle-manifest.json     what the runner runs, and what it leaves out and why
  tests\                   kGraf's test programs, built for Windows
  programs\                kgraf.exe and the helper programs the tests start
  src\                     the source files and fixtures the tests read
