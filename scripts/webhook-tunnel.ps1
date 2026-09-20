# webhook-tunnel.ps1 -- starts ngrok pointed at a local daemon port and
# reports its public HTTPS tunnel URL. Used only by build-debug.cmd's
# --webhook-tunnel flag: ngrok is a dev-only external tool, never a project
# dependency, so this script errors out clearly if it's not on PATH instead
# of trying to fetch/vendor it.
#
# Writes the started ngrok process's PID to -PidFile and the discovered
# public URL to -UrlFile (mirroring build-debug.cmd's own daemon-PID-via-
# temp-file handoff) rather than printing them, so the caller doesn't have
# to parse this script's stdout.

param(
    [Parameter(Mandatory = $true)][int]$Port,
    [Parameter(Mandatory = $true)][string]$PidFile,
    [Parameter(Mandatory = $true)][string]$UrlFile
)

$ngrok = Get-Command ngrok -ErrorAction SilentlyContinue
if (-not $ngrok) {
    Write-Error "ngrok is not on PATH -- install it from https://ngrok.com/download (a dev-only tool for --webhook-tunnel, not a project dependency)"
    exit 1
}

# -NoNewWindow, not -WindowStyle Hidden -- same reasoning build-debug.cmd's
# own comment gives for the daemon's own Start-Process call: a hidden window
# still gets its own console/process group, which a Ctrl-C typed into the
# calling terminal never reaches. -NoNewWindow keeps ngrok attached to the
# console this script (and build-debug.cmd, and the daemon) all share, so
# one Ctrl-C tears down all three together. No -RedirectStandardOutput/
# -RedirectStandardError for the same reason the daemon's own call omits
# them -- ngrok's own status output just flows into the same shared
# console instead.
$proc = Start-Process -FilePath $ngrok.Source -ArgumentList @('http', "$Port") -PassThru -NoNewWindow
Set-Content -Encoding ascii -Path $PidFile -Value $proc.Id

# ngrok's own local status API (127.0.0.1:4040, loopback-only, no auth) --
# polled rather than scraped from ngrok's console output, since the log
# line format isn't a stable contract and this API is (see
# https://ngrok.com/docs/agent/api). Filters on proto == "https" rather than
# string-matching public_url, since a plain "ngrok http" tunnel can report
# both an http and an https tunnel for the same session.
$url = $null
for ($i = 0; $i -lt 30 -and -not $url; $i++) {
    Start-Sleep -Milliseconds 500
    try {
        $tunnel = (Invoke-RestMethod "http://127.0.0.1:4040/api/tunnels" -TimeoutSec 1).tunnels |
            Where-Object { $_.proto -eq 'https' } | Select-Object -First 1
        if ($tunnel) { $url = $tunnel.public_url }
    } catch {
        # Not up yet (connection refused) or a transient hiccup -- keep polling.
    }
}

if (-not $url) {
    Write-Error "ngrok did not report a public https:// tunnel within 15s (check http://127.0.0.1:4040, or ngrok's own console output above)"
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    exit 1
}

Set-Content -Encoding ascii -Path $UrlFile -Value $url
