<#
.SYNOPSIS
  Opens the live dashboard in a new Windows Terminal window and captures real screenshots of
  that window only (PrintWindow), once mid-run and once at completion.
#>
param(
    [string]$OutDir = "results\suite\screenshots",
    [string]$Model = "phi3",
    [string]$Urls = "http://127.0.0.1:11435,http://127.0.0.1:11436",
    [int]$Events = 60,
    [int]$MidSeconds = 6,
    [int]$FinalSeconds = 45,
    [int]$Cols = 150,
    [int]$Rows = 48,
    # Full launcher script body to run instead of the default single-shot `run` (e.g. an `agent` run).
    [string]$Launch = "",
    [string]$Prefix = "dashboard"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$out = Join-Path $root $OutDir
New-Item -ItemType Directory -Force $out | Out-Null

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @"
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class WinShot {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    public static void Save(IntPtr h, string path) {
        RECT r; GetWindowRect(h, out r);
        using (var bmp = new Bitmap(r.R - r.L, r.B - r.T, PixelFormat.Format32bppArgb))
        using (var g = Graphics.FromImage(bmp)) {
            IntPtr hdc = g.GetHdc();
            PrintWindow(h, hdc, 2); // PW_RENDERFULLCONTENT: works for DirectX-composited windows
            g.ReleaseHdc(hdc);
            bmp.Save(path, ImageFormat.Png);
        }
    }
}
"@
[WinShot]::SetProcessDPIAware() | Out-Null

$title = "InversionGap-Dashboard-$PID"
$exe = Join-Path $root "target\release\inversion-gap.exe"
# Windows PowerShell's Start-Process does not quote array elements containing spaces, so the
# dashboard command goes into a launcher file rather than onto wt's command line.
$launcher = Join-Path $out "launch-$Prefix.ps1"
$body = if ($Launch) { $Launch.Replace("{exe}", $exe) } else { "& '$exe' run --model $Model --ollama-url $Urls --events $Events" }
Set-Content $launcher -Encoding utf8 -Value $body
Start-Process wt.exe -ArgumentList "-w new --size $Cols,$Rows --title $title --suppressApplicationTitle -d `"$root`" powershell -NoProfile -NoExit -ExecutionPolicy Bypass -File `"$launcher`""

$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 60 -and $hwnd -eq [IntPtr]::Zero; $i++) {
    Start-Sleep -Milliseconds 250
    $hwnd = [WinShot]::FindWindow("CASCADIA_HOSTING_WINDOW_CLASS", $title)
}
if ($hwnd -eq [IntPtr]::Zero) { throw "Windows Terminal window '$title' not found" }
[WinShot]::SetForegroundWindow($hwnd) | Out-Null

Start-Sleep -Seconds $MidSeconds
[WinShot]::Save($hwnd, (Join-Path $out "$Prefix-midrun.png"))
Write-Host "captured mid-run"
Start-Sleep -Seconds ($FinalSeconds - $MidSeconds)
[WinShot]::Save($hwnd, (Join-Path $out "$Prefix-final.png"))
Write-Host "captured final"
[WinShot]::PostMessage($hwnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null  # WM_CLOSE
Write-Host "screenshots -> $out"
