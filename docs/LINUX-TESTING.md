# Testing kGraf on Linux

## Linux self-test in GitHub Actions

kGraf's Linux sandbox needs unprivileged user namespaces. Dependency installs limited to
kGraf's local egress proxy (the only network route a sandboxed install has), and hidden
folders inside a writable tree, both depend on them. Ubuntu 23.10 and later block user namespaces for ordinary programs
(`kernel.apparmor_restrict_unprivileged_userns=1`) unless an AppArmor profile grants the
`userns` permission to that program. The kGraf `.deb` carries such a profile for the
installed program.

The `linuxtest.yml` workflow tests this on a real Ubuntu 24.04 machine.

### Start it

```
gh workflow run linuxtest.yml
gh run watch
gh run download <run id> -n kgraf-linux-selftest
```

The workflow has a manual trigger only. It takes about 30 minutes, most of it the release
build of the package, and stops after 60. The repository is public, so standard runners
cost nothing. The artifact is kept for 14 days.

Two optional inputs:

- `-f apparmor_profile=NAME`: the profile name to look for in `aa-status`. By default the
  script reads the name from the profile file in the package (`dpkg -L kgraf`), and falls
  back to the file name.
- `-f selfcheck_bin=PATH`: the installed program that runs the check. Default
  `/usr/bin/kgraf`.

### What it runs

| Step | What it does | Why |
| --- | --- | --- |
| Build | Builds the `.deb` with `KGRAF_LINUX_NATIVE=1 scripts/package-linux.sh` and the test binaries of `kgraf-sandbox` and `kgraf-host`. | The package is the thing a user installs. |
| Facts | Records the kernel, the restriction sysctl, `aa-status --enabled`, the AppArmor version, and whether `unshare -Ur true` works for the runner user, before any change. | The result depends on the image. Without the facts a pass or fail cannot be read. |
| Part A | Makes an ordinary user, `kgtest`, with no sudo. Sets the restriction off. Runs the `egress`, `escape_linux`, `workspace`, `merge` and `line_endings` test binaries of `kgraf-sandbox`, and the `deps::install::tests::` tests of `kgraf-host`, as `kgtest`. | The unprivileged user namespace path, which a root container never takes. |
| Part B | Sets the restriction on. Installs the `.deb` with `dpkg -i`. Checks that the profile is loaded (`aa-status`). Runs `kgraf sandbox selfcheck` as `kgtest`. It must pass. | The state of a user on Ubuntu 23.10 or later who installed the package. |
| Part C | Unloads the profile with `apparmor_parser -R`. Runs the same check. It must fail, and the message must name the fix. Reloads the profile. | A machine where the profile is missing must get a clear refusal, not a weaker sandbox. |

`kgraf sandbox selfcheck` is a hidden command (it is not in `--help`). It runs `/bin/sh`
in the sandbox with network only through the egress proxy and a hidden folder inside the
writable tree. It checks four things: the command writes in the tree; it cannot write in
the hidden folder; it reaches an allowed local origin through the proxy; a direct
connection to the same origin fails. It needs `curl`. On a system that denies user
namespaces it exits with the sandbox's refusal.

Part A runs the test binaries from the build folder, not from a copy. A test binary holds
the absolute path of its source folder, and its tests read fixtures there. The script makes
the folders above the checkout searchable and the checkout readable for `kgtest` instead.
Each test runs with a clean environment (`env -i`), `HOME` and `TMPDIR` in the home folder
of `kgtest`, and `--test-threads=2`.

### Read the report

The artifact holds `report.md` and a `logs/` folder. The same report appears in the run
summary. `report.md` has these sections:

1. Facts. Read the restriction value first. It shows whether the image restricts user
   namespaces before the job changes anything. Parts A and B set it themselves.
2. Setup. The ordinary user and its sudo rights (none).
3. Part A to Part C. Each test group shows the command, the exit code, PASS or FAIL, the
   libtest summary lines (`test result: ...`), and the last 4 KB of output when it failed.

Rules for reading it:

- A group is PASS when its exit code is 0. A test that printed `skipped:` passed without
  testing anything, for example when user namespaces were unavailable. The report lists
  each such line under a WARNING. Treat a group with a WARNING in Part A as a failure.
- Part B passes when the installed program prints `sandbox selfcheck: ok`. If the report
  says `loaded in the kernel: NO`, the package installed no loaded profile, and the
  failure is in the package.
- Part C passes when the exit code is not 0 and the output mentions user namespaces. The
  line `the message names AppArmor, the sysctl or the profile as the fix` must read `yes`.
  If it reads `no`, the refusal does not tell the user what to do.
- The control line `unshare -Ur true` as `kgtest` in Part B should show a non-zero exit
  when the restriction is on. It proves the restriction is active, so that Part B passes
  because of the profile and not by accident.

The job fails only when the build fails or when no report is written. A failing part is a
result in the report, not a red job.

### What it cannot cover

- The desktop app window and tray icon. The runner has no desktop session.
- Other distributions and other AppArmor versions. It runs on Ubuntu 24.04 only. Debian,
  Fedora and Arch have no such restriction by default, and SELinux is not tested.
- A machine that has no Node.js, npm, Git or curl. The runner image has them.
- A machine with its own AppArmor profiles for other programs.
- An ARM machine. The runner is x86_64.
- The runner account is not a real user: it is created by the image and may differ from a
  user's account in groups and limits. `kgtest` is a plain account made for the test.

The job changes a sysctl, installs a package and unloads a profile. It is safe on an
ephemeral runner. Do not run `scripts/linuxtest.sh` on a machine you use.
