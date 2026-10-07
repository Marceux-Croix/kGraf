# Windows Firewall network test for the kGraf self-test bundle.
#
# Usage (elevated PowerShell):
#   scripts/wintest-firewall.ps1 -Bundle <staged bundle folder> -Out <text file>
#
# It copies curl.exe to a private folder, runs it as the current user and
# through `kgraf-wintest.exe sandbox-run --`, adds an outbound block rule
# scoped to that copy and the current user, and runs both again. The rule, the
# folder and any firewall profile the script enabled are always restored in
# `finally`. The script reports what it saw and exits 0; the output file is
# the result.
param(
    [Parameter(Mandatory = $true)][string]$Bundle,
    [Parameter(Mandatory = $true)][string]$Out
)
$ErrorActionPreference = 'Continue'
$dir = 'C:\kgraf-fwtest'
$rule = 'kgraf-fwtest'
$runner = Join-Path (Resolve-Path $Bundle).Path 'kgraf-wintest.exe'
$enabledByUs = @()
$lines = New-Object System.Collections.Generic.List[string]
function Say([string]$s) { Write-Host $s; $lines.Add($s) }

# Runs a program with the curl request, optionally behind a prefix command.
function Run-Curl {
    param([string[]]$Prefix = @())
    $all = @($Prefix) + @("$dir\curl.exe", '-sS', '-o', 'NUL', '-w', '%{http_code}', '--max-time', '15', 'https://example.com')
    $text = & $all[0] $all[1..($all.Count - 1)] 2>&1 | Out-String
    return ($text.Trim() + "  (exit code $LASTEXITCODE)")
}
function Try-Both {
    Say ('you:     ' + (Run-Curl))
    Say ('sandbox: ' + (Run-Curl -Prefix @($runner, 'sandbox-run', '--')))
}

try {
    $id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object System.Security.Principal.WindowsPrincipal($id)
    $admin = $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
    $sid = $id.User.Value
    $kind = if ($sid -like 'S-1-5-21-*') { 'local or domain account' } else { 'other account type' }
    Say "date (UTC): $((Get-Date).ToUniversalTime().ToString('o'))"
    Say "user: $($id.Name)"
    Say "elevated (administrator role): $admin"
    Say "SID kind: $kind"
    Say "OS: $((Get-CimInstance Win32_OperatingSystem).Caption) $([Environment]::OSVersion.Version)"
    Say 'firewall profiles before the test:'
    $profiles = @(Get-NetFirewallProfile)
    foreach ($p in $profiles) { Say "  $($p.Name): Enabled=$($p.Enabled)" }

    foreach ($p in $profiles) {
        if ("$($p.Enabled)" -ne 'True') {
            Set-NetFirewallProfile -Name $p.Name -Enabled True
            $enabledByUs += $p.Name
            Say "profile $($p.Name) was not enabled: enabled for this test only, restored at the end"
        }
    }

    New-Item -ItemType Directory $dir -Force | Out-Null
    Copy-Item C:\Windows\System32\curl.exe "$dir\curl.exe"

    Say '--- before the rule'
    Try-Both
    New-NetFirewallRule -DisplayName $rule -Direction Outbound -Action Block `
        -Program "$dir\curl.exe" -LocalUser "D:(A;;CC;;;$sid)" | Out-Null
    Say '--- with the rule'
    Try-Both
}
catch {
    Say "script error: $_"
}
finally {
    Remove-NetFirewallRule -DisplayName $rule -ErrorAction SilentlyContinue
    if (Test-Path $dir) { Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
    foreach ($name in $enabledByUs) {
        Set-NetFirewallProfile -Name $name -Enabled False
        Say "profile $name restored to its earlier state (not enabled)"
    }
    Say '--- cleaned up'
    $lines | Set-Content -Path $Out -Encoding utf8
}
exit 0
