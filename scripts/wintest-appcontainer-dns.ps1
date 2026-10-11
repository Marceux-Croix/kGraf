# AppContainer DNS probe for the kGraf Windows test workflow.
#
# Usage (PowerShell, as the runner's own account):
#   scripts/wintest-appcontainer-dns.ps1 -Out <markdown file> [-RawDir <folder>]
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
#   A. look up the probe name inside the container, two ways: the DNS Client
#      service API (DnsQuery_W, which `Resolve-DnsName -DnsOnly` also uses) and
#      getaddrinfo (Dns.GetHostAddresses);
#   B. the same two lookups outside the container, as a control;
#   C. run git and cargo inside the container.
# Resolve-DnsName itself does not run in the container: the DnsClient module
# is built on CIM, which needs WMI, and an AppContainer cannot reach WMI. The
# lookup therefore calls DnsQuery_W directly through Add-Type.
#
# The probe name is example.com. A name made up for each run would not
# resolve, and then a leaked lookup (NXDOMAIN) and a refused one would look
# the same to getaddrinfo. The packets are matched on the exact name, and the
# process that sent a query comes from the DNS Client event log, so the name
# does not have to be unique.
#
# Raw data is kept in -RawDir (default: appcontainer-dns-raw next to -Out):
# per phase the pktmon .etl, its text form and a pcapng from `pktmon
# etl2pcap`, and an export of the DNS-Client/Operational event log. The
# markdown lists every captured DNS packet (time, direction, frame size, name)
# after removing the copies of one packet seen at several points of the
# network stack, and every DNS Client event in the probe window.
#
# The process is created with Win32 calls (userenv.dll, kernel32.dll) through
# Add-Type. NtObjectManager is installed and its version recorded, but the
# probe does not depend on its cmdlet names, which are not checked here.
#
# The script catches every error, reports it as a result line, and exits 0.
# The profile, the access grants, the capture filters, the event log state and
# the temp folder are restored or removed in `finally`.
param(
    [Parameter(Mandatory = $true)][string]$Out,
    [string]$RawDir = ''
)
$ErrorActionPreference = 'Continue'
$profileName = 'kgraf-dns-probe'
$work = 'C:\kgraf-acprobe'
$probeName = 'example.com'
$dnsLog = 'Microsoft-Windows-DNS-Client/Operational'
if (-not $RawDir) { $RawDir = Join-Path ([System.IO.Path]::GetDirectoryName($Out)) 'appcontainer-dns-raw' }
$lines = New-Object System.Collections.Generic.List[string]
$grants = New-Object System.Collections.Generic.List[string]
$script:sid = $null
$script:profileMade = $false
$script:logWas = $null      # 'true' or 'false' once the event log state is read
$script:logChanged = $false
$script:probeStart = $null
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

    // Process ID of the last process Run created, so a result can name it.
    public static int LastPid;

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
            LastPid = pi.dwProcessId;
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

# Decoder for the pcapng files that `pktmon etl2pcap` writes. It lists the DNS
# packets (UDP or TCP, port 53) with time, frame size, direction and the first
# question name. A packet on port 853 (DNS over TLS) or one that does not
# decode is listed with no name. One packet is logged at several points of the
# network stack; copies with the same DNS ID, kind, name, type and ports within
# 100 ms of the last copy count as one packet.
$csPcap = @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Text;

public class PcapRow {
    public DateTime Time;
    public string Dir = "";
    public int Frame;
    public string Proto = "";
    public string Src = "";
    public string Dst = "";
    public string Name;
    public string QType = "";
    public string Kind = "";
    public bool Decoded;
}

public class PcapResult {
    public List<PcapRow> Rows = new List<PcapRow>();
    public int Records;
    public int Duplicates;
    public int EmptyTcp;
    public string Error = "";
}

public static class PcapDns {
    static uint Le32(byte[] b, int o) {
        return (uint)b[o] | ((uint)b[o + 1] << 8) | ((uint)b[o + 2] << 16) | ((uint)b[o + 3] << 24);
    }
    static int Le16(byte[] b, int o) { return b[o] | (b[o + 1] << 8); }
    static int N16(byte[] b, int o) { return (b[o] << 8) | b[o + 1]; }

    public static PcapResult Parse(string path) {
        var res = new PcapResult();
        try { ParseInner(path, res); }
        catch (Exception e) { res.Error = e.GetType().Name + ": " + e.Message; }
        return res;
    }

    static void ParseInner(string path, PcapResult res) {
        byte[] d = File.ReadAllBytes(path);
        var links = new List<int>();
        var tsres = new List<double>();
        var seen = new Dictionary<string, DateTime>();
        var epoch = new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc);
        int pos = 0;
        while (pos + 12 <= d.Length) {
            uint type = Le32(d, pos);
            int len = (int)Le32(d, pos + 4);
            if (len < 12 || pos + len > d.Length) throw new InvalidDataException("bad block length " + len + " at offset " + pos);
            int body = pos + 8, bodyEnd = pos + len - 4;
            if (type == 0x0A0D0D0Au) {
                if (Le32(d, body) != 0x1A2B3C4Du) throw new InvalidDataException("byte order is not little-endian");
                links.Clear(); tsres.Clear();
            } else if (type == 1u) {
                links.Add(Le16(d, body));
                double r = 1e6; // if_tsresol default: microseconds
                int o = body + 8;
                while (o + 4 <= bodyEnd) {
                    int code = Le16(d, o), olen = Le16(d, o + 2);
                    if (code == 0) break;
                    if (code == 9 && olen >= 1 && o + 4 < bodyEnd) {
                        int v = d[o + 4];
                        r = (v & 0x80) != 0 ? Math.Pow(2, v & 0x7F) : Math.Pow(10, v);
                    }
                    o += 4 + ((olen + 3) & ~3);
                }
                tsres.Add(r);
            } else if (type == 6u) {
                res.Records++;
                int ifid = (int)Le32(d, body);
                ulong ts = ((ulong)Le32(d, body + 4) << 32) | Le32(d, body + 8);
                int cap = (int)Le32(d, body + 12), orig = (int)Le32(d, body + 16);
                int data = body + 20;
                if (cap < 0 || data + cap > bodyEnd) throw new InvalidDataException("bad packet length at offset " + pos);
                int link = ifid >= 0 && ifid < links.Count ? links[ifid] : 1;
                double r = ifid >= 0 && ifid < tsres.Count ? tsres[ifid] : 1e6;
                DateTime t = epoch.AddTicks((long)((double)ts / r * 1e7));
                int dirBits = 0; // epb_flags: 1 inbound, 2 outbound, 0 unknown
                int o = data + ((cap + 3) & ~3);
                while (o + 4 <= bodyEnd) {
                    int code = Le16(d, o), olen = Le16(d, o + 2);
                    if (code == 0) break;
                    if (code == 2 && olen >= 4 && o + 8 <= bodyEnd) dirBits = (int)(Le32(d, o + 4) & 3u);
                    o += 4 + ((olen + 3) & ~3);
                }
                AddPacket(res, seen, d, data, cap, orig, link, t, dirBits);
            }
            pos += len;
        }
    }

    static string Ip(byte[] d, int o, int n) {
        var b = new byte[n];
        Array.Copy(d, o, b, 0, n);
        return new System.Net.IPAddress(b).ToString();
    }

    static void Emit(PcapResult res, Dictionary<string, DateTime> seen, PcapRow row, string key) {
        DateTime prev;
        if (seen.TryGetValue(key, out prev) && Math.Abs((row.Time - prev).TotalMilliseconds) < 100.0) {
            res.Duplicates++;
            seen[key] = row.Time;
            return;
        }
        seen[key] = row.Time;
        res.Rows.Add(row);
    }

    static string DirText(int dirBits, bool toServer) {
        if (dirBits == 1) return "in";
        if (dirBits == 2) return "out";
        return toServer ? "out*" : "in*"; // * = inferred from the ports, not logged
    }

    static void AddPacket(PcapResult res, Dictionary<string, DateTime> seen, byte[] d, int start, int cap,
                          int orig, int link, DateTime t, int dirBits) {
        int p = start, end = start + cap;
        int et = 0;
        if (link == 1) {
            if (end - p >= 14) {
                et = N16(d, p + 12); p += 14;
                while ((et == 0x8100 || et == 0x88A8) && end - p >= 4) { et = N16(d, p + 2); p += 4; }
            }
        } else if (link == 101 && end - p >= 1) {
            int v = d[p] >> 4;
            et = v == 4 ? 0x0800 : (v == 6 ? 0x86DD : 0);
        }
        string src = "", dst = "";
        int proto = -1, l4 = -1;
        if (et == 0x0800 && end - p >= 20) {
            int ihl = (d[p] & 0xF) * 4;
            if (ihl >= 20 && end - p >= ihl) { proto = d[p + 9]; src = Ip(d, p + 12, 4); dst = Ip(d, p + 16, 4); l4 = p + ihl; }
        } else if (et == 0x86DD && end - p >= 40) {
            proto = d[p + 6]; src = Ip(d, p + 8, 16); dst = Ip(d, p + 24, 16); l4 = p + 40;
        }
        var row = new PcapRow();
        row.Time = t; row.Frame = orig;
        int sport = 0, dport = 0, ps = -1;
        if (proto == 17 && end - l4 >= 8) {
            row.Proto = "udp"; sport = N16(d, l4); dport = N16(d, l4 + 2); ps = l4 + 8;
        } else if (proto == 6 && end - l4 >= 20) {
            row.Proto = "tcp"; sport = N16(d, l4); dport = N16(d, l4 + 2);
            int off = (d[l4 + 12] >> 4) * 4;
            ps = l4 + off;
            if (off < 20 || ps > end) ps = end;
        } else {
            row.Proto = "other"; row.Src = src; row.Dst = dst; row.Kind = "not udp or tcp";
            row.Dir = DirText(dirBits, true);
            Emit(res, seen, row, "o|" + et + "|" + orig);
            return;
        }
        row.Src = src + ":" + sport; row.Dst = dst + ":" + dport;
        int payload = end - ps;
        if (row.Proto == "tcp" && payload <= 0) { res.EmptyTcp++; return; }
        bool toServer = dport == 53 || dport == 853;
        row.Dir = DirText(dirBits, toServer);
        if ((sport == 53 || dport == 53) && payload > 0) {
            int q = ps;
            if (row.Proto == "tcp" && payload >= 14 && N16(d, ps) <= payload - 2) q = ps + 2;
            int id;
            if (DecodeDns(d, q, end, row, out id)) {
                row.Decoded = true;
                if (dirBits == 0) row.Dir = row.Kind.StartsWith("query") ? "out*" : "in*";
                Emit(res, seen, row, id + "|" + row.Kind + "|" + row.Name + "|" + row.QType + "|" + row.Proto + "|" + sport + "|" + dport);
                return;
            }
        }
        row.Kind = (sport == 853 || dport == 853) ? "port 853, payload not decoded" : "payload not decoded";
        Emit(res, seen, row, "u|" + row.Proto + "|" + sport + "|" + dport + "|" + orig);
    }

    static string TypeName(int t) {
        switch (t) {
            case 1: return "A";
            case 2: return "NS";
            case 5: return "CNAME";
            case 12: return "PTR";
            case 15: return "MX";
            case 16: return "TXT";
            case 28: return "AAAA";
            case 33: return "SRV";
            case 65: return "HTTPS";
            case 255: return "ANY";
            default: return "type" + t;
        }
    }

    static bool DecodeDns(byte[] d, int p, int end, PcapRow row, out int id) {
        id = -1;
        if (end - p < 13) return false;
        id = N16(d, p);
        int flags = N16(d, p + 2), qd = N16(d, p + 4);
        if (qd < 1) return false;
        int o = p + 12;
        var sb = new StringBuilder();
        while (true) {
            if (o >= end) return false;
            int l = d[o];
            if (l == 0) { o++; break; }
            if ((l & 0xC0) != 0 || o + 1 + l > end) return false;
            if (sb.Length > 0) sb.Append('.');
            for (int i = 0; i < l; i++) {
                byte c = d[o + 1 + i];
                sb.Append(c >= 32 && c < 127 ? (char)c : '?');
            }
            o += 1 + l;
        }
        if (o + 4 > end) return false;
        row.Name = sb.Length == 0 ? "." : sb.ToString().ToLowerInvariant();
        row.QType = TypeName(N16(d, o));
        row.Kind = (flags & 0x8000) != 0 ? "response rcode " + (flags & 0xF) : "query";
        return true;
    }
}
'@

# The lookups run as script files so that the same text runs in the container
# and in the control. The DnsQuery_W lookup carries its C# as base64, which
# avoids nested here-string quoting; the container compiles it with Add-Type
# (Windows PowerShell 5.1, csc.exe). A compile failure prints NOTRUN, so it is
# not read as a refused lookup.
$csDnsq = @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class DnsProbe {
    [DllImport("dnsapi.dll", CharSet = CharSet.Unicode)]
    static extern int DnsQuery_W(string name, ushort type, uint options, IntPtr extra, out IntPtr results, IntPtr reserved);
    [DllImport("dnsapi.dll")]
    static extern void DnsRecordListFree(IntPtr list, int freeType);

    // Asks the DNS Client service for A records. Returns "ADDR:<list>" or "ERR:<text>".
    public static string Query(string name) {
        // BYPASS_CACHE 0x8, NO_HOSTS_FILE 0x40, NO_NETBT 0x80, NO_MULTICAST 0x800:
        // only a DNS server can answer.
        uint options = 0x8u | 0x40u | 0x80u | 0x800u;
        IntPtr list;
        int status = DnsQuery_W(name, 1, options, IntPtr.Zero, out list, IntPtr.Zero);
        if (status != 0) {
            return "ERR:DnsQuery_W status " + status + " (" + new Win32Exception(status).Message + ")";
        }
        var addrs = new List<string>();
        try {
            // DNS_RECORD: pNext, pName, wType (2), wDataLength (2), flags (4), ttl (4), reserved (4), data.
            int dataOff = 2 * IntPtr.Size + 16;
            for (IntPtr p = list; p != IntPtr.Zero; p = Marshal.ReadIntPtr(p)) {
                int type = Marshal.ReadInt16(p, 2 * IntPtr.Size) & 0xFFFF;
                if (type == 1) {
                    uint v = unchecked((uint)Marshal.ReadInt32(p, dataOff));
                    addrs.Add(new System.Net.IPAddress((long)v).ToString());
                }
            }
        } finally {
            if (list != IntPtr.Zero) DnsRecordListFree(list, 1);
        }
        if (addrs.Count == 0) return "ERR:DnsQuery_W status 0 but no A record";
        return "ADDR:" + string.Join(",", addrs.ToArray());
    }
}
'@
$dnsqB64 = [Convert]::ToBase64String([System.Text.Encoding]::UTF8.GetBytes($csDnsq))
$dnsqScript = @'
param([string]$Name)
try {
    $src = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('__B64__'))
    Add-Type -TypeDefinition $src -Language CSharp
}
catch { Write-Output ("NOTRUN:" + $_.Exception.Message); exit 2 }
try {
    $s = [DnsProbe]::Query($Name)
    Write-Output $s
    if ($s -notlike 'ADDR:*') { exit 1 }
}
catch { Write-Output ("ERR:" + $_.Exception.Message); exit 1 }
'@ -replace '__B64__', $dnsqB64
$gaiScript = @'
param([string]$Name)
try { $r = [System.Net.Dns]::GetHostAddresses($Name); Write-Output ("ADDR:" + (($r | ForEach-Object { $_.ToString() }) -join ",")) }
catch { Write-Output ("ERR:" + $_.Exception.Message); exit 1 }
'@

# Runs a command line in the container and returns an object with the exit
# code (or the creation error), stdout and stderr text, and the process ID.
function Invoke-InContainer([string]$Cmd, [string]$Dir, [hashtable]$Env = @{}, [int]$TimeoutSec = 60) {
    $n = [guid]::NewGuid().ToString('N').Substring(0, 8)
    $o = Join-Path $work "out-$n.txt"
    $e = Join-Path $work "err-$n.txt"
    $r = [ordered]@{ Started = $false; Exit = $null; Error = ''; Out = ''; Err = ''; Pid = $null }
    # The container cannot write the runner's temp folder, so point TEMP at the work folder.
    $envs = @{ TEMP = $work; TMP = $work }
    foreach ($k in $Env.Keys) { $envs[$k] = $Env[$k] }
    try {
        $r.Exit = [AcProbe]::Run($script:sid, $Cmd, $Dir, $envs, $o, $e, $TimeoutSec * 1000)
        $r.Started = $true
        $r.Pid = [AcProbe]::LastPid
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

$psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
# Lookup name -> script file; filled in once the work folder exists.
$lookups = [ordered]@{}

function Keep([string]$Path) {
    if (Test-Path $Path) { Copy-Item $Path $RawDir -Force -ErrorAction SilentlyContinue }
}

# Pktmon capture around a script block. Returns the block's value and the
# capture: the packet count from the text form (every appearance of a packet at
# every stack point), the decoded, de-duplicated DNS packets from the pcapng,
# and the UTC window. The raw files are copied to $RawDir before the work
# folder goes. A capture that cannot run has Count = $null.
function With-Capture([string]$Tag, [scriptblock]$Body) {
    $etl = Join-Path $work "cap-$Tag.etl"
    $txt = Join-Path $work "cap-$Tag.txt"
    $pcap = Join-Path $work "cap-$Tag.pcapng"
    $res = [ordered]@{
        Count = $null; Note = ''; Rows = @(); PcapOk = $false; PcapNote = ''
        Records = 0; Duplicates = 0; EmptyTcp = 0; Start = [datetime]::UtcNow; End = [datetime]::UtcNow
    }
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
            $res.End = [datetime]::UtcNow
            & pktmon etl2txt $etl -o $txt 2>&1 | Out-Null
            if (Test-Path $txt) {
                $pk = @(Select-String -Path $txt -Pattern 'PktNumber' -ErrorAction SilentlyContinue)
                $res.Count = $pk.Count
            }
            else { $res.Note = '(pktmon etl2txt produced no text file)' }
            & pktmon etl2pcap $etl -o $pcap 2>&1 | Out-Null
            if (Test-Path $pcap) {
                $pr = [PcapDns]::Parse($pcap)
                if ($pr.Error) { $res.PcapNote = "pcapng decode failed: $($pr.Error)" }
                else {
                    $res.PcapOk = $true
                    $res.Rows = @($pr.Rows | Sort-Object Time)
                    $res.Records = $pr.Records; $res.Duplicates = $pr.Duplicates; $res.EmptyTcp = $pr.EmptyTcp
                }
            }
            elseif ($res.Count -eq 0) {
                # Nothing was captured, so no pcapng is expected.
                $res.PcapOk = $true
            }
            else { $res.PcapNote = 'pktmon etl2pcap produced no file' }
            Keep $etl; Keep $txt; Keep $pcap
        }
    }
    return [pscustomobject]@{ Value = $value; Capture = [pscustomobject]$res }
}

# Classifies one lookup result as answered, refused, not-run (the lookup code
# could not be built) or not-started (no process).
function Classify($r) {
    if (-not $r.Started) { return @{ State = 'not-started'; Detail = $r.Error } }
    $nr = [regex]::Match($r.Out, 'NOTRUN:(.*)')
    if ($nr.Success) { return @{ State = 'not-run'; Detail = "the lookup code did not build: $($nr.Groups[1].Value.Trim())" } }
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

function Same-Name($row) {
    return ($null -ne $row.Name) -and ($row.Name.TrimEnd('.').ToLowerInvariant() -eq $probeName)
}

# Reads the DNS Client events in a UTC window. Ok is $false when the log could
# not be read; an empty log is Ok with no rows.
function Get-DnsEvents([datetime]$StartUtc, [datetime]$EndUtc, $WinA, $WinB) {
    $out = [ordered]@{ Ok = $true; Note = ''; Rows = @() }
    $ev = @()
    try {
        $ev = @(Get-WinEvent -FilterHashtable @{ LogName = $dnsLog; StartTime = $StartUtc.ToLocalTime(); EndTime = $EndUtc.ToLocalTime() } -ErrorAction Stop)
    }
    catch {
        if ($_.FullyQualifiedErrorId -like 'NoMatchingEventsFound*') { $ev = @() }
        else { $out.Ok = $false; $out.Note = "could not read the event log: $($_.Exception.Message)"; return [pscustomobject]$out }
    }
    $ns = New-Object System.Xml.XmlNamespaceManager((New-Object System.Xml.XmlDocument).NameTable)
    $ns.AddNamespace('e', 'http://schemas.microsoft.com/win/2004/08/events/event')
    $rows = New-Object System.Collections.Generic.List[object]
    foreach ($e in ($ev | Sort-Object TimeCreated)) {
        $x = [xml]$e.ToXml()
        $data = @{}
        foreach ($n in $x.SelectNodes('//e:EventData/e:Data', $ns)) { $data[$n.GetAttribute('Name')] = $n.InnerText }
        $exec = $x.SelectSingleNode('//e:System/e:Execution', $ns)
        $execPid = if ($exec) { $exec.GetAttribute('ProcessID') } else { '' }
        $name = ''
        foreach ($k in 'QueryName', 'Name', 'NameQuery') { if ($data.ContainsKey($k)) { $name = $data[$k]; break } }
        $statusKey = @($data.Keys | Where-Object { $_ -like '*Status*' } | Select-Object -First 1)
        $status = if ($statusKey.Count -gt 0) { $data[$statusKey[0]] } else { '' }
        $pidKey = @($data.Keys | Where-Object { $_ -match '^(Process|PID)' } | Select-Object -First 1)
        $dataPid = if ($pidKey.Count -gt 0) { $data[$pidKey[0]] } else { '' }
        $tu = $e.TimeCreated.ToUniversalTime()
        $phase = '-'
        if ($tu -ge $WinA.Start -and $tu -le $WinA.End) { $phase = 'A' }
        elseif ($tu -ge $WinB.Start -and $tu -le $WinB.End) { $phase = 'B' }
        $rows.Add([pscustomobject]@{ Time = $tu; Phase = $phase; Id = $e.Id; Name = $name; Pid = $execPid; DataPid = $dataPid; Status = $status })
    }
    $out.Rows = $rows.ToArray()
    return [pscustomobject]$out
}

function Say-Packets($Cap, [string]$Title) {
    Say ''
    Say "### $Title"
    if (-not $Cap.PcapOk) { Say "- packet list unavailable: $($Cap.PcapNote)"; return }
    Say "- pcapng records: $($Cap.Records); copies of one packet removed: $($Cap.Duplicates); TCP segments with no data skipped: $($Cap.EmptyTcp); packets listed: $($Cap.Rows.Count)"
    if ($Cap.Rows.Count -eq 0) { Say '- no DNS packet captured'; return }
    Say '- direction: in or out as logged; a star means inferred from the ports'
    Say '| time (UTC) | dir | frame bytes | proto | from | to | kind | type | name |'
    Say '|---|---|---|---|---|---|---|---|---|'
    foreach ($r in $Cap.Rows) {
        $nm = if ($r.Decoded) { $r.Name } else { '(not decoded)' }
        Say ("| {0} | {1} | {2} | {3} | {4} | {5} | {6} | {7} | {8} |" -f $r.Time.ToString('HH:mm:ss.ffffff'), $r.Dir, $r.Frame, $r.Proto, $r.Src, $r.Dst, $r.Kind, $r.QType, $nm)
    }
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
    Say "probe name: $probeName"

    # Installed for follow-up work. A failed install is not an error for the probe.
    try {
        Install-Module NtObjectManager -Scope CurrentUser -Force -ErrorAction Stop
        $m = Get-Module -ListAvailable NtObjectManager | Sort-Object Version -Descending | Select-Object -First 1
        Say "NtObjectManager: installed, version $($m.Version) (not used by the probe)"
    }
    catch { Say "NtObjectManager: install failed: $($_.Exception.Message)" }

    if (Test-Path $work) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
    New-Item -ItemType Directory $work -Force | Out-Null
    New-Item -ItemType Directory $RawDir -Force | Out-Null
    Add-Type -TypeDefinition $cs -Language CSharp
    Add-Type -TypeDefinition $csPcap -Language CSharp

    $dnsqFile = Join-Path $work 'lookup-dnsquery.ps1'
    $gaiFile = Join-Path $work 'lookup-getaddrinfo.ps1'
    Set-Content -Path $dnsqFile -Value $dnsqScript -Encoding utf8
    Set-Content -Path $gaiFile -Value $gaiScript -Encoding utf8
    $lookups['DnsQuery_W (DNS Client service API)'] = $dnsqFile
    $lookups['getaddrinfo (Dns.GetHostAddresses)'] = $gaiFile

    # The DNS Client operational log records query names and process IDs. It is
    # often off; turn it on for the probe and put it back in `finally`.
    $gl = & wevtutil gl $dnsLog 2>&1 | Out-String
    $en = [regex]::Match($gl, '(?im)^\s*enabled:\s*(true|false)')
    if ($en.Success) {
        $script:logWas = $en.Groups[1].Value.ToLower()
        if ($script:logWas -eq 'false') {
            & wevtutil sl $dnsLog /e:true 2>&1 | Out-Null
            $script:logChanged = ($LASTEXITCODE -eq 0)
        }
        Say "event log $dnsLog : was enabled=$($script:logWas), enabled for the probe: $($script:logChanged -or $script:logWas -eq 'true')"
    }
    else { Say "event log $dnsLog : state not readable: $(First-Lines $gl 2)" }

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
    Say "raw files are kept in the artifact folder $(Split-Path -Leaf $RawDir)"

    $script:probeStart = [datetime]::UtcNow

    # Phase A: inside the container.
    $phaseA = With-Capture 'A' {
        $o = [ordered]@{}
        foreach ($k in $lookups.Keys) {
            $cmd = "`"$psExe`" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$($lookups[$k])`" $probeName"
            $o[$k] = Invoke-InContainer $cmd $work
        }
        $o
    }
    # Phase B: control, outside the container.
    $phaseB = With-Capture 'B' {
        $o = [ordered]@{}
        foreach ($k in $lookups.Keys) {
            $t = & $psExe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $lookups[$k] $probeName 2>&1 | Out-String
            $o[$k] = [pscustomobject]@{ Started = $true; Exit = $LASTEXITCODE; Error = ''; Out = $t; Err = ''; Pid = $null }
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
            $r = $p.Ph.Value[$k]
            $c = Classify $r
            $states["$($p.Tag)|$k"] = $c.State
            $pidText = if ($r.Pid) { ", process ID $($r.Pid)" } else { '' }
            Say "- $k : $($c.State.ToUpper()), $($c.Detail)$pidText"
        }
        $cap = $p.Ph.Capture
        $cnt = if ($null -eq $cap.Count) { 'unknown' } else { $cap.Count }
        Say "- packets on port 53/853 in the text form (each packet appears at several stack points): $cnt $($cap.Note)"
        Say "- capture window (UTC): $($cap.Start.ToString('HH:mm:ss.fff')) to $($cap.End.ToString('HH:mm:ss.fff'))"
        Say-Packets $cap "Phase $($p.Tag) packets"
    }

    # DNS Client events for the probe window. The log is written
    # asynchronously, so wait a moment first.
    Start-Sleep -Seconds 3
    $evEnd = [datetime]::UtcNow.AddSeconds(1)
    $events = Get-DnsEvents $script:probeStart $evEnd $phaseA.Capture $phaseB.Capture
    Say ''
    Say "## DNS Client events in the probe window ($dnsLog)"
    if (-not $events.Ok) { Say "- $($events.Note)" }
    else {
        Say "- events: $($events.Rows.Count)"
        $containerPids = @($phaseA.Value.Values | ForEach-Object { $_.Pid } | Where-Object { $_ } | ForEach-Object { [string]$_ })
        Say "- process IDs of the in-container processes the probe started in phase A: $($containerPids -join ', ')"
        Say '- pid is the process ID in the event header; data pid is a process field in the event data, if the event has one'
        if ($events.Rows.Count -gt 0) {
            Say '| time (UTC) | phase | event | name | pid | data pid | status |'
            Say '|---|---|---|---|---|---|---|'
            foreach ($r in ($events.Rows | Select-Object -First 400)) {
                Say ("| {0} | {1} | {2} | {3} | {4} | {5} | {6} |" -f $r.Time.ToString('HH:mm:ss.ffffff'), $r.Phase, $r.Id, $r.Name, $r.Pid, $r.DataPid, $r.Status)
            }
            if ($events.Rows.Count -gt 400) { Say "- table cut at 400 rows; the exported log has all $($events.Rows.Count)" }
        }
    }
    # Export the log for the window with wevtutil (the same events, in the
    # original format).
    $evtx = Join-Path $work 'dns-client-operational.evtx'
    $q = "*[System[TimeCreated[@SystemTime>='$($script:probeStart.AddSeconds(-1).ToString('o'))' and @SystemTime<='$($evEnd.ToString('o'))']]]"
    & wevtutil epl $dnsLog $evtx "/q:$q" 2>&1 | Out-Null
    if (Test-Path $evtx) { Keep $evtx; Say "- exported the log for the window to $(Split-Path -Leaf $evtx)" }
    else { Say '- export of the log failed' }

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
        $detail = if ($r.Started) { "process ID $($r.Pid); exit $($r.Exit); stdout: $(First-Lines $r.Out 2); stderr: $(First-Lines $r.Err 4)" } else { $r.Error }
        Say "- $($t.Name) : $detail"
    }

    # Process start and verdict.
    Say ''
    Say '## Result'
    Say "in-container processes started: $anyStarted"
    $aStates = @($lookups.Keys | ForEach-Object { $states["A|$_"] })
    $aCap = $phaseA.Capture
    $bCap = $phaseB.Capture
    $aNamed = @($aCap.Rows | Where-Object { Same-Name $_ })
    $bNamed = @($bCap.Rows | Where-Object { Same-Name $_ })
    $aUndecoded = @($aCap.Rows | Where-Object { -not $_.Decoded })
    $aContainerHits = @()
    if ($events.Ok) {
        $aContainerHits = @($events.Rows | Where-Object {
                $_.Phase -eq 'A' -and $_.Name.TrimEnd('.').ToLowerInvariant() -eq $probeName -and
                (($containerPids -contains [string]$_.Pid) -or ($containerPids -contains [string]$_.DataPid)) -and
                $_.Status -match '^(0|0x0+)$'
            })
    }
    Say "phase A packets that carry the name $probeName : $($aNamed.Count)"
    Say "phase B (control) packets that carry the name $probeName : $($bNamed.Count)"
    Say "phase A packets that did not decode (no name): $($aUndecoded.Count)"
    Say "phase A DNS Client events with status 0 for the name from an in-container process ID: $($aContainerHits.Count)"
    $verdict = 'INCONCLUSIVE'
    $why = 'inputs did not match a defined outcome'
    if ($aStates -contains 'answered') { $verdict = 'LEAK-SERVICE'; $why = 'a container lookup answered' }
    elseif ($aNamed.Count -gt 0) { $verdict = 'FALLBACK-SENT'; $why = "$($aNamed.Count) phase A packet(s) carry the name $probeName" }
    elseif (-not $aCap.PcapOk) { $why = "no usable packet list for phase A: $($aCap.PcapNote)" }
    elseif ($aUndecoded.Count -gt 0) { $why = "$($aUndecoded.Count) phase A packet(s) did not decode, so their names are unknown" }
    elseif ($aStates -contains 'not-started') { $why = 'a phase A process did not start' }
    elseif ($aStates -contains 'not-run') { $why = 'a phase A lookup did not run: its code did not build in the container' }
    elseif (-not $bCap.PcapOk -or $bNamed.Count -eq 0) { $why = 'the control sent no packet with the name, so the capture may not work' }
    elseif (-not $events.Ok) { $why = "no DNS Client events: $($events.Note)" }
    elseif ($aContainerHits.Count -gt 0) { $why = 'the DNS Client log shows a successful query from an in-container process ID, but no lookup answered and no packet carries the name' }
    else { $verdict = 'CONFINED'; $why = "no phase A packet carries $probeName, no in-container process ID has a successful query in the DNS Client log, and the control sent it $($bNamed.Count) time(s)" }
    Say "VERDICT: $verdict ($why)"
    Say 'note: the capture sees the whole machine. A packet with the name in phase A may come from another process; the event table above gives the process ID.'
    Say 'note: the DNS Client log may give the service process ID in the header. If no event pid matches an in-container process ID, attribution rests on the timing and the packets.'
}
catch {
    Say "script error: $_"
    Say 'VERDICT: INCONCLUSIVE (script error)'
}
finally {
    try { & pktmon stop 2>&1 | Out-Null } catch { }
    try { & pktmon filter remove 2>&1 | Out-Null } catch { }
    if ($script:logChanged) {
        try { & wevtutil sl $dnsLog /e:false 2>&1 | Out-Null; Say "event log $dnsLog restored to enabled=false: exit $LASTEXITCODE" }
        catch { Say "event log restore error: $($_.Exception.Message)" }
    }
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
