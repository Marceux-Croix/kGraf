# AppContainer DNS probe for the kGraf Windows test workflow.
#
# Usage (PowerShell, as the runner's own account):
#   scripts/wintest-appcontainer-dns.ps1 -Out <markdown file>
#
# Question: can a capability-less AppContainer process resolve names? The DNS
# Client service (Dnscache) answers local RPC callers and sends the query
# itself, so a Windows Filtering Platform (WFP) rule on the caller cannot stop
# it. Its RPC server is documented to admit AppContainer callers only when the
# token holds a network capability. This script tests that on the real
# machine. It also tests whether git and cargo run inside the container.
#
# Steps: make a capability-less AppContainer profile, capture ports 53 and 853
# with pktmon in each phase, then
#   A. look up example.com inside the container, two ways;
#   B. the same lookups outside the container, as a control;
#   C. run git and cargo inside the container.
# The process is created with Win32 calls (userenv.dll, kernel32.dll) through
# Add-Type. NtObjectManager is installed and its version recorded, but the
# probe does not depend on its cmdlet names, which are not checked here.
#
# The script catches every error, reports it as a result line, and exits 0.
# The profile, the access grants, the capture filters and the temp folder are
# removed in `finally`.
param(
    [Parameter(Mandatory = $true)][string]$Out
)
$ErrorActionPreference = 'Continue'
$profileName = 'kgraf-dns-probe'
$work = 'C:\kgraf-acprobe'
$lines = New-Object System.Collections.Generic.List[string]
$grants = New-Object System.Collections.Generic.List[string]
$script:sid = $null
$script:profileMade = $false
function Say([string]$s) { Write-Host $s; $lines.Add($s) }

$cs = @'
using System;
using System.Collections;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class AcProbe {
    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_CAPABILITIES {
        public IntPtr AppContainerSid; public IntPtr Capabilities;
        public uint CapabilityCount; public uint Reserved;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct SECURITY_ATTRIBUTES {
        public int nLength; public IntPtr lpSecurityDescriptor; public bool bInheritHandle;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct STARTUPINFO {
        public int cb; public string lpReserved, lpDesktop, lpTitle;
        public int dwX, dwY, dwXSize, dwYSize, dwXCountChars, dwYCountChars, dwFillAttribute;
        public int dwFlags; public short wShowWindow, cbReserved2;
        public IntPtr lpReserved2, hStdInput, hStdOutput, hStdError;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct STARTUPINFOEX { public STARTUPINFO StartupInfo; public IntPtr lpAttributeList; }
    [StructLayout(LayoutKind.Sequential)]
    struct PROCESS_INFORMATION { public IntPtr hProcess, hThread; public int dwProcessId, dwThreadId; }

    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int CreateAppContainerProfile(string name, string display, string desc,
        IntPtr caps, uint capCount, out IntPtr sid);
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int DeriveAppContainerSidFromAppContainerName(string name, out IntPtr sid);
    [DllImport("userenv.dll", CharSet = CharSet.Unicode)]
    static extern int DeleteAppContainerProfile(string name);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertSidToStringSid(IntPtr sid, out string s);
    [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool ConvertStringSidToSid(string s, out IntPtr sid);
    [DllImport("advapi32.dll")]
    static extern IntPtr FreeSid(IntPtr sid);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool InitializeProcThreadAttributeList(IntPtr l, int n, int f, ref IntPtr size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateProcThreadAttribute(IntPtr l, uint f, IntPtr attr, IntPtr v,
        IntPtr size, IntPtr prev, IntPtr ret);
    [DllImport("kernel32.dll")]
    static extern void DeleteProcThreadAttributeList(IntPtr l);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern bool CreateProcessW(string app, StringBuilder cmd, IntPtr pa, IntPtr ta,
        bool inherit, uint flags, IntPtr env, string dir, ref STARTUPINFOEX si, out PROCESS_INFORMATION pi);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr CreateFileW(string name, uint access, uint share,
        ref SECURITY_ATTRIBUTES sa, uint disp, uint flags, IntPtr tmpl);
    [DllImport("kernel32.dll")] static extern uint WaitForSingleObject(IntPtr h, uint ms);
    [DllImport("kernel32.dll")] static extern bool GetExitCodeProcess(IntPtr h, out uint code);
    [DllImport("kernel32.dll")] static extern bool TerminateProcess(IntPtr h, uint code);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);

    // Creates the capability-less profile (or reuses it) and returns its package SID.
    public static string EnsureProfile(string name) {
        IntPtr sid;
        int hr = CreateAppContainerProfile(name, name, "kGraf DNS probe", IntPtr.Zero, 0, out sid);
        if (hr == unchecked((int)0x800700B7)) hr = DeriveAppContainerSidFromAppContainerName(name, out sid);
        if (hr != 0) throw new Win32Exception(hr & 0xFFFF, "profile HRESULT 0x" + hr.ToString("X8"));
        string s;
        if (!ConvertSidToStringSid(sid, out s)) throw new Win32Exception();
        FreeSid(sid);
        return s;
    }
    public static int DeleteProfile(string name) { return DeleteAppContainerProfile(name); }

    static IntPtr Open(string path, bool write) {
        var sa = new SECURITY_ATTRIBUTES(); sa.nLength = Marshal.SizeOf(sa); sa.bInheritHandle = true;
        // GENERIC_WRITE or GENERIC_READ, share read and write, CREATE_ALWAYS or OPEN_EXISTING.
        IntPtr h = CreateFileW(path, write ? 0x40000000u : 0x80000000u, 3, ref sa,
            write ? 2u : 3u, 0x80, IntPtr.Zero);
        if (h == new IntPtr(-1)) throw new Win32Exception();
        return h;
    }
    static IntPtr EnvBlock(Hashtable over) {
        var env = new SortedDictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (DictionaryEntry e in Environment.GetEnvironmentVariables()) env[(string)e.Key] = (string)e.Value;
        // Values from PowerShell can arrive wrapped (PSObject); ToString gives the string they hold.
        foreach (DictionaryEntry e in over) env[Convert.ToString(e.Key)] = Convert.ToString(e.Value);
        var sb = new StringBuilder();
        foreach (var kv in env) sb.Append(kv.Key).Append('=').Append(kv.Value).Append('\0');
        sb.Append('\0');
        return Marshal.StringToHGlobalUni(sb.ToString());
    }

    // Runs cmd inside the AppContainer. Output goes to outFile and errFile, which
    // the container must be able to write. Returns the exit code, or -1 on a
    // timeout. Throws when the process cannot be created.
    public static int Run(string sidText, string cmd, string dir, Hashtable envOver,
                          string outFile, string errFile, int timeoutMs) {
        IntPtr sid;
        if (!ConvertStringSidToSid(sidText, out sid)) throw new Win32Exception();
        IntPtr caps = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(SECURITY_CAPABILITIES)));
        var sc = new SECURITY_CAPABILITIES(); sc.AppContainerSid = sid;
        Marshal.StructureToPtr(sc, caps, false);
        IntPtr size = IntPtr.Zero;
        InitializeProcThreadAttributeList(IntPtr.Zero, 1, 0, ref size);
        IntPtr list = Marshal.AllocHGlobal(size);
        IntPtr hIn = IntPtr.Zero, hOut = IntPtr.Zero, hErr = IntPtr.Zero, env = IntPtr.Zero;
        bool listOk = false;
        try {
            if (!InitializeProcThreadAttributeList(list, 1, 0, ref size)) throw new Win32Exception();
            listOk = true;
            // PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES
            if (!UpdateProcThreadAttribute(list, 0, (IntPtr)0x20009, caps,
                    (IntPtr)Marshal.SizeOf(typeof(SECURITY_CAPABILITIES)), IntPtr.Zero, IntPtr.Zero))
                throw new Win32Exception();
            hIn = Open("NUL", false); hOut = Open(outFile, true); hErr = Open(errFile, true);
            env = EnvBlock(envOver);
            var si = new STARTUPINFOEX();
            si.StartupInfo.cb = Marshal.SizeOf(typeof(STARTUPINFOEX));
            si.StartupInfo.dwFlags = 0x100; // STARTF_USESTDHANDLES
            si.StartupInfo.hStdInput = hIn; si.StartupInfo.hStdOutput = hOut; si.StartupInfo.hStdError = hErr;
            si.lpAttributeList = list;
            PROCESS_INFORMATION pi;
            // EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW
            if (!CreateProcessW(null, new StringBuilder(cmd), IntPtr.Zero, IntPtr.Zero, true,
                    0x80000u | 0x400u | 0x08000000u, env, dir, ref si, out pi))
                throw new Win32Exception();
            CloseHandle(pi.hThread);
            try {
                if (WaitForSingleObject(pi.hProcess, (uint)timeoutMs) != 0) {
                    TerminateProcess(pi.hProcess, 1);
                    return -1;
                }
                uint code;
                GetExitCodeProcess(pi.hProcess, out code);
                return unchecked((int)code);
            } finally { CloseHandle(pi.hProcess); }
        } finally {
            if (listOk) DeleteProcThreadAttributeList(list);
            Marshal.FreeHGlobal(list); Marshal.FreeHGlobal(caps);
            if (hIn != IntPtr.Zero) CloseHandle(hIn);
            if (hOut != IntPtr.Zero) CloseHandle(hOut);
            if (hErr != IntPtr.Zero) CloseHandle(hErr);
            if (env != IntPtr.Zero) Marshal.FreeHGlobal(env);
        }
    }
}
'@

# Runs a command line in the container and returns an object with the exit
# code (or the creation error), stdout and stderr text.
function Invoke-InContainer([string]$Cmd, [string]$Dir, [hashtable]$Env = @{}, [int]$TimeoutSec = 60) {
    $n = [guid]::NewGuid().ToString('N').Substring(0, 8)
    $o = Join-Path $work "out-$n.txt"
    $e = Join-Path $work "err-$n.txt"
    $r = [ordered]@{ Started = $false; Exit = $null; Error = ''; Out = ''; Err = '' }
    # The container cannot write the runner's temp folder, so point TEMP at the work folder.
    $envs = @{ TEMP = $work; TMP = $work }
    foreach ($k in $Env.Keys) { $envs[$k] = $Env[$k] }
    try {
        $r.Exit = [AcProbe]::Run($script:sid, $Cmd, $Dir, $envs, $o, $e, $TimeoutSec * 1000)
        $r.Started = $true
        if ($r.Exit -eq -1) { $r.Error = "timeout after $TimeoutSec s" }
    }
    catch { $r.Error = "could not start: $($_.Exception.Message)" }
    if (Test-Path $o) { $r.Out = (Get-Content -Raw $o -ErrorAction SilentlyContinue) }
    if (Test-Path $e) { $r.Err = (Get-Content -Raw $e -ErrorAction SilentlyContinue) }
    if ($null -eq $r.Out) { $r.Out = '' }
    if ($null -eq $r.Err) { $r.Err = '' }
    return [pscustomobject]$r
}

function First-Lines([string]$t, [int]$n = 4) {
    $l = @($t -split "`r?`n" | Where-Object { $_.Trim() -ne '' } | Select-Object -First $n)
    return ($l -join ' | ')
}

# Two lookups as PowerShell scripts. Output is ADDR:<list> or ERR:<text>.
$psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$lookups = [ordered]@{
    'Resolve-DnsName -DnsOnly' = 'try { $r = Resolve-DnsName example.com -DnsOnly -ErrorAction Stop; Write-Output ("ADDR:" + ((@($r | Where-Object { $_.IPAddress } | ForEach-Object { $_.IPAddress })) -join ",")) } catch { Write-Output ("ERR:" + $_.Exception.Message); exit 1 }'
    'getaddrinfo (Dns.GetHostAddresses)' = 'try { $r = [System.Net.Dns]::GetHostAddresses("example.com"); Write-Output ("ADDR:" + (($r | ForEach-Object { $_.ToString() }) -join ",")) } catch { Write-Output ("ERR:" + $_.Exception.Message); exit 1 }'
}

# Pktmon capture around a script block. Returns the block's value and the
# matching packet count with a few sample lines. A capture that cannot run
# has Count = $null.
function With-Capture([string]$Tag, [scriptblock]$Body) {
    $etl = Join-Path $work "cap-$Tag.etl"
    $txt = Join-Path $work "cap-$Tag.txt"
    $res = [ordered]@{ Count = $null; Sample = ''; Note = '' }
    $started = $false
    try {
        & pktmon start --capture --pkt-size 0 -f $etl 2>&1 | Out-Null
        $started = ($LASTEXITCODE -eq 0)
        if (-not $started) { $res.Note = "(pktmon start failed, exit $LASTEXITCODE)" }
    }
    catch { $res.Note = "(pktmon start error: $($_.Exception.Message))" }
    # An answer already in the DNS Client cache needs no query on the wire, so
    # each phase starts from an empty cache.
    try { Clear-DnsClientCache -ErrorAction Stop } catch { $res.Note += " (cache flush failed: $($_.Exception.Message))" }
    $value = $null
    try { $value = & $Body }
    finally {
        if ($started) {
            Start-Sleep -Seconds 2
            & pktmon stop 2>&1 | Out-Null
            & pktmon etl2txt $etl -o $txt 2>&1 | Out-Null
            if (Test-Path $txt) {
                $pk = @(Select-String -Path $txt -Pattern 'PktNumber' -ErrorAction SilentlyContinue)
                $res.Count = $pk.Count
                $res.Sample = (($pk | Select-Object -First 3 | ForEach-Object { $_.Line.Trim() }) -join ' | ')
            }
            else { $res.Note = '(pktmon etl2txt produced no text file)' }
        }
    }
    return [pscustomobject]@{ Value = $value; Capture = [pscustomobject]$res }
}

# Classifies one lookup result as refused, answered or not-started.
function Classify($r) {
    if (-not $r.Started) { return @{ State = 'not-started'; Detail = $r.Error } }
    $addr = [regex]::Match($r.Out, 'ADDR:(\S*)')
    if ($addr.Success -and $addr.Groups[1].Value -ne '') {
        return @{ State = 'answered'; Detail = "addresses $($addr.Groups[1].Value)" }
    }
    $err = [regex]::Match($r.Out, 'ERR:(.*)')
    $text = if ($err.Success) { $err.Groups[1].Value.Trim() } else { (First-Lines ($r.Out + ' ' + $r.Err) 3) }
    if ($r.Exit -eq -1) { $text = $r.Error }
    return @{ State = 'refused'; Detail = "error: $text (exit $($r.Exit))" }
}

function Grant([string]$Path, [string]$Rights) {
    if (-not $Path -or -not (Test-Path $Path)) { Say "grant skipped, path missing: $Path"; return }
    & icacls $Path /grant "*$($script:sid):(OI)(CI)$Rights" 2>&1 | Out-Null
    Say "grant $Rights on ${Path}: icacls exit $LASTEXITCODE"
    if ($LASTEXITCODE -eq 0) { $grants.Add($Path) }
}

$phaseA = $null; $phaseB = $null
try {
    Say '# AppContainer DNS probe'
    Say ''
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    Say "date (UTC): $((Get-Date).ToUniversalTime().ToString('o'))"
    Say "user: $($id.Name)"
    Say "Windows build: $((Get-CimInstance Win32_OperatingSystem).Caption) $([Environment]::OSVersion.Version)"
    Say "PowerShell: $($PSVersionTable.PSVersion)"

    # Installed for follow-up work. A failed install is not an error for the probe.
    try {
        Install-Module NtObjectManager -Scope CurrentUser -Force -ErrorAction Stop
        $m = Get-Module -ListAvailable NtObjectManager | Sort-Object Version -Descending | Select-Object -First 1
        Say "NtObjectManager: installed, version $($m.Version) (not used by the probe)"
    }
    catch { Say "NtObjectManager: install failed: $($_.Exception.Message)" }

    if (Test-Path $work) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory $work -Force | Out-Null
    Add-Type -TypeDefinition $cs -Language CSharp

    $script:sid = [AcProbe]::EnsureProfile($profileName)
    $script:profileMade = $true
    Say "AppContainer profile: $profileName, package SID $($script:sid), no capabilities"
    Grant $work 'F'

    # Capture filters: UDP and TCP, ports 53 and 853 (a filter with no
    # protocol matches both).
    & pktmon filter remove 2>&1 | Out-Null
    & pktmon filter add kgraf53 -p 53 2>&1 | Out-Null
    & pktmon filter add kgraf853 -p 853 2>&1 | Out-Null
    Say 'pktmon filters: port 53, port 853'

    # Phase A: inside the container.
    $phaseA = With-Capture 'A' {
        $o = [ordered]@{}
        foreach ($k in $lookups.Keys) {
            $arg = $lookups[$k] -replace '"', '\"'
            $o[$k] = Invoke-InContainer "`"$psExe`" -NoProfile -NonInteractive -Command `"$arg`"" $work
        }
        $o
    }
    # Phase B: control, outside the container.
    $phaseB = With-Capture 'B' {
        $o = [ordered]@{}
        foreach ($k in $lookups.Keys) {
            $t = & $psExe -NoProfile -NonInteractive -Command $lookups[$k] 2>&1 | Out-String
            $o[$k] = [pscustomobject]@{ Started = $true; Exit = $LASTEXITCODE; Error = ''; Out = $t; Err = '' }
        }
        $o
    }

    $states = @{}
    $phases = @(
        @{ Tag = 'A'; Title = 'inside the container'; Ph = $phaseA },
        @{ Tag = 'B'; Title = 'outside the container (control)'; Ph = $phaseB }
    )
    foreach ($p in $phases) {
        Say ''
        Say "## Phase $($p.Tag): lookups $($p.Title)"
        foreach ($k in $lookups.Keys) {
            $c = Classify $p.Ph.Value[$k]
            $states["$($p.Tag)|$k"] = $c.State
            Say "- $k : $($c.State.ToUpper()), $($c.Detail)"
        }
        $cap = $p.Ph.Capture
        $cnt = if ($null -eq $cap.Count) { 'unknown' } else { $cap.Count }
        Say "- packets on port 53/853: $cnt $($cap.Note)"
        if ($cap.Sample) { Say "- sample: $($cap.Sample)" }
    }

    # Phase C: developer tools inside the container.
    Say ''
    Say '## Phase C: tools inside the container'
    $gitCmd = Get-Command git -ErrorAction SilentlyContinue
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
    $gitRoot = $null
    if ($gitCmd) { $gitRoot = Split-Path (Split-Path $gitCmd.Source -Parent) -Parent }
    Grant $cargoHome 'RX'
    Grant $rustupHome 'RX'
    Grant $gitRoot 'RX'

    $repo = Join-Path $work 'repo'
    New-Item -ItemType Directory $repo -Force | Out-Null
    & git -C $repo init -q 2>&1 | Out-Null
    Set-Content (Join-Path $repo 'a.txt') 'x'
    $crate = Join-Path $work 'tiny'
    New-Item -ItemType Directory (Join-Path $crate 'src') -Force | Out-Null
    Set-Content (Join-Path $crate 'Cargo.toml') "[package]`nname = `"tiny`"`nversion = `"0.1.0`"`nedition = `"2021`"`n"
    Set-Content (Join-Path $crate 'src\main.rs') 'fn main() { println!("ok"); }'

    $cEnv = @{ CARGO_HOME = $cargoHome; RUSTUP_HOME = $rustupHome; CARGO_TARGET_DIR = (Join-Path $crate 'target') }
    $tools = @(
        @{ Name = 'git --version'; Dir = $work; T = 60 },
        @{ Name = 'git status'; Dir = $repo; T = 60 },
        @{ Name = 'cargo --version'; Dir = $work; T = 60 },
        @{ Name = 'cargo build'; Dir = $crate; T = 600 }
    )
    $anyStarted = (@($states.Values | Where-Object { $_ -ne 'not-started' })).Count -gt 0
    foreach ($t in $tools) {
        # cmd.exe resolves git and cargo through PATH, which the container inherits.
        $r = Invoke-InContainer "cmd.exe /c $($t.Name)" $t.Dir $cEnv $t.T
        if ($r.Started) { $anyStarted = $true }
        $detail = if ($r.Started) { "exit $($r.Exit); stdout: $(First-Lines $r.Out 2); stderr: $(First-Lines $r.Err 4)" } else { $r.Error }
        Say "- $($t.Name) : $detail"
    }

    # Process start and verdict.
    Say ''
    Say '## Result'
    Say "in-container processes started: $anyStarted"
    $aStates = @($lookups.Keys | ForEach-Object { $states["A|$_"] })
    $aCount = $phaseA.Capture.Count
    $bCount = $phaseB.Capture.Count
    $verdict = 'INCONCLUSIVE'
    $why = 'inputs did not match a defined outcome'
    if ($aStates -contains 'not-started') { $why = 'a phase A process did not start' }
    elseif ($aStates -contains 'answered') { $verdict = 'LEAK-SERVICE'; $why = 'a container lookup answered' }
    elseif ($null -eq $aCount) { $why = 'no packet count for phase A' }
    elseif ($aCount -gt 0) { $verdict = 'FALLBACK-SENT'; $why = "lookups failed but $aCount packets matched port 53/853" }
    elseif (-not $bCount) { $why = 'both lookups refused with 0 packets, but the control saw no packets, so the capture may not work' }
    else { $verdict = 'CONFINED'; $why = "both lookups refused and 0 packets in phase A (control saw $bCount)" }
    Say "VERDICT: $verdict ($why)"
    Say 'note: phase A packets include any other port 53/853 traffic on the machine during the capture.'
}
catch {
    Say "script error: $_"
    Say 'VERDICT: INCONCLUSIVE (script error)'
}
finally {
    try { & pktmon stop 2>&1 | Out-Null } catch { }
    try { & pktmon filter remove 2>&1 | Out-Null } catch { }
    foreach ($g in $grants) {
        & icacls $g /remove "*$($script:sid)" 2>&1 | Out-Null
        Say "grant removed on ${g}: icacls exit $LASTEXITCODE"
    }
    if ($script:profileMade) {
        try { $hr = [AcProbe]::DeleteProfile($profileName); Say "profile removed: HRESULT $hr" }
        catch { Say "profile removal error: $($_.Exception.Message)" }
    }
    if (Test-Path $work) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
    Say '--- cleaned up'
    try { $lines | Set-Content -Path $Out -Encoding utf8 } catch { Write-Host "could not write ${Out}: $_" }
}
exit 0
