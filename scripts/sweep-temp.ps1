<#
.SYNOPSIS
  Removes stale ralphus test leftovers that make Windows logon slow (RAL-691).

.DESCRIPTION
  The Windows profile service walks every file under %LOCALAPPDATA%\Temp at each
  logon, so a large Temp costs minutes of logon time (see .agent/gotchas.md).
  This one-shot tool deletes allow-listed ralphus directories only:

    * run roots `<pid>-<start-secs>` under <Root>\ralphus-tests and under the
      test-tmp folder (%LOCALAPPDATA%\ralphus\test-tmp or RALPHUS_TEST_TMP) whose
      pid is dead or that are older than -OlderThanHours
    * legacy flat directories in <Root>: ^ral\d+-, ^ralphus-, ^ral-agent-profiles-,
      ^wt-rec-  (older than -OlderThanHours)
    * with -IncludeThirdParty: node-compile-cache, pytest-of-*, 8.3-named .NET dirs

  Never touched: Temp\claude, Temp\ralphus (exact), Temp\ralphus-runner-io, and
  anything modified in the last 2 hours (a test may be running).

  Deleting a large backlog takes tens of minutes (about 1,400 entries/s measured);
  run it in a background window. Progress is logged every 500 directories and the
  final line is `deleted=N failed=M`; every failure is printed.

.PARAMETER DryRun
  List what would be deleted, delete nothing.
.PARAMETER OlderThanHours
  Age threshold in hours (default 24). Values under 2 are raised to 2.
.PARAMETER Root
  Temp folder to sweep (default $env:LOCALAPPDATA\Temp).
.PARAMETER TestTmpRoot
  Test-tmp folder holding run roots (default $env:RALPHUS_TEST_TMP, else
  $env:LOCALAPPDATA\ralphus\test-tmp). Derived from -Root's parent when -Root is
  given explicitly and this is not, so a scratch -Root never reaches the real folder.
#>
[CmdletBinding()]
param(
    [switch]$DryRun,
    [double]$OlderThanHours = 24,
    [string]$Root,
    [string]$TestTmpRoot,
    [switch]$IncludeThirdParty
)

$ErrorActionPreference = 'Continue'
$minGuardHours = 2
if ($OlderThanHours -lt $minGuardHours) { $OlderThanHours = $minGuardHours }

if (-not $Root) {
    $Root = Join-Path $env:LOCALAPPDATA 'Temp'
    if (-not $TestTmpRoot) {
        $TestTmpRoot = if ($env:RALPHUS_TEST_TMP) { $env:RALPHUS_TEST_TMP } else { Join-Path $env:LOCALAPPDATA 'ralphus\test-tmp' }
    }
}
if (-not (Test-Path -LiteralPath $Root)) { Write-Host "Root not found: $Root"; exit 1 }

$now = Get-Date
$ageCutoff = $now.AddHours(-$OlderThanHours)
$guardCutoff = $now.AddHours(-$minGuardHours)
$neverNames = @('claude', 'ralphus', 'ralphus-runner-io')

function Test-Allowed([string]$name) {
    if ($neverNames -contains $name) { return $false }
    if ($name -match '^ral\d+-' -or $name -match '^ralphus-' -or
        $name -match '^ral-agent-profiles-' -or $name -match '^wt-rec-') { return $true }
    if ($IncludeThirdParty -and ($name -eq 'node-compile-cache' -or $name -like 'pytest-of-*' -or
            $name -match '^[0-9a-f]{8}\.[0-9a-z]{3}$')) { return $true }
    return $false
}

function Test-RunRootDead([string]$name) {
    if ($name -notmatch '^(\d+)-(\d+)$') { return $false }
    return -not (Get-Process -Id ([int]$Matches[1]) -ErrorAction SilentlyContinue)
}

$targets = New-Object System.Collections.Generic.List[string]

# Legacy flat families in Root (ralphus-tests is a run-root parent, handled below).
Get-ChildItem -LiteralPath $Root -Directory -Force -ErrorAction SilentlyContinue | ForEach-Object {
    if ($_.Name -eq 'ralphus-tests') { return }
    if ((Test-Allowed $_.Name) -and $_.LastWriteTime -lt $ageCutoff) { $targets.Add($_.FullName) }
}

# Run roots: <pid>-<start-secs>, dead pid or older than the threshold.
$runParents = @((Join-Path $Root 'ralphus-tests'))
if ($TestTmpRoot) { $runParents += $TestTmpRoot }
foreach ($parent in $runParents) {
    if (-not (Test-Path -LiteralPath $parent)) { continue }
    Get-ChildItem -LiteralPath $parent -Directory -Force -ErrorAction SilentlyContinue | ForEach-Object {
        if ($_.Name -notmatch '^\d+-\d+$') { return }
        if ($_.LastWriteTime -ge $guardCutoff) { return }
        if ((Test-RunRootDead $_.Name) -or $_.LastWriteTime -lt $ageCutoff) { $targets.Add($_.FullName) }
    }
}

Write-Host ("candidates={0} root={1} olderThanHours={2} dryRun={3}" -f $targets.Count, $Root, $OlderThanHours, [bool]$DryRun)

function Remove-Tree([string]$path) {
    try { [IO.Directory]::Delete($path, $true); return $true } catch { }
    cmd /c "rd /s /q `"$path`"" 2>$null | Out-Null
    if (-not (Test-Path -LiteralPath $path)) { return $true }
    cmd /c "attrib -r -s -h `"$path`" /s /d" 2>$null | Out-Null
    cmd /c "rd /s /q `"$path`"" 2>$null | Out-Null
    return -not (Test-Path -LiteralPath $path)
}

$deleted = 0
$failed = 0
$i = 0
foreach ($p in $targets) {
    $i++
    if ($DryRun) { Write-Host "would delete $p"; continue }
    if (Remove-Tree $p) { $deleted++ } else { $failed++; Write-Host "FAILED $p" }
    if ($i % 500 -eq 0) { Write-Host ("progress {0}/{1} deleted={2} failed={3}" -f $i, $targets.Count, $deleted, $failed) }
}

if ($DryRun) { Write-Host ("dryrun candidates={0}" -f $targets.Count) }
else { Write-Host "deleted=$deleted failed=$failed" }
if ($failed -gt 0) { exit 2 }
