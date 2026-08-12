# Live end-to-end check of the Rust trainer against a running RDR2.
#
# MUST be run as Administrator: RDR2 runs at a privilege level a normal process
# cannot open, so OpenProcess fails with ERROR_ACCESS_DENIED otherwise.
#
#   Right-click PowerShell -> "Run as Administrator", then:
#     cd <repo root>
#     .\tests\verify-rdr2-live.ps1
#
# Enable order matters. "Main CodeBase" is this table's master script: it runs
# 14 AOB scans and registers 35 symbols (_pPlayer, _GodMode, _pStaminaBase, ...)
# that almost every other feature addresses. Enabling a child first leaves its
# guard comparing against a zeroed allocation, so the feature silently does
# nothing - which is exactly how this table behaves in CE too.

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$trainer = Join-Path $root 'dist\rdr2_trainer.exe'
$api = 'http://127.0.0.1:42069'

function Fail($m) { Write-Host "FAIL  $m" -ForegroundColor Red }
function Pass($m) { Write-Host "PASS  $m" -ForegroundColor Green }
function Info($m) { Write-Host "      $m" -ForegroundColor DarkGray }
function Head($m) { Write-Host "`n== $m" -ForegroundColor Cyan }

function Toggle($desc) {
    $body = @{ description = $desc; active = $true } | ConvertTo-Json -Compress
    try {
        Invoke-RestMethod -Uri "$api/api/features/toggle" -Method Post `
            -ContentType 'application/json' -Body $body -TimeoutSec 120
    } catch { @{ error = $_.Exception.Message } }
}
function DebugRead($expr, $size) {
    $e = [uri]::EscapeDataString($expr)
    try { Invoke-RestMethod -Uri "$api/api/debug/read?expr=$e&size=$size" -TimeoutSec 10 }
    catch { $null }
}
function Flatten($nodes) {
    foreach ($n in $nodes) { $n; if ($n.children) { Flatten $n.children } }
}
function Coords() {
    $f = Invoke-RestMethod -Uri "$api/api/features" -TimeoutSec 20
    $all = Flatten $f.features
    $all | Where-Object { $_.description -in @('Coord - X', 'Coord - Y', 'Coord - Z') } |
        Select-Object -First 3
}

# --- preconditions --------------------------------------------------------
Head 'Preconditions'
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Fail 'Not elevated. RDR2 cannot be opened without Administrator.'; exit 1
}
Pass 'Running elevated'

$game = Get-Process -Name RDR2 -ErrorAction SilentlyContinue
if (-not $game) { Fail 'RDR2.exe is not running'; exit 1 }
Pass "RDR2 running (PID $($game.Id))"
if (-not (Test-Path $trainer)) { Fail "Missing $trainer"; exit 1 }

# --- launch ---------------------------------------------------------------
Head 'Trainer'
Get-Process rdr2_trainer, trainer-runtime -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
Start-Process -FilePath $trainer -WindowStyle Hidden

$features = $null
foreach ($i in 1..30) {
    Start-Sleep -Seconds 1
    try { $features = Invoke-RestMethod -Uri "$api/api/features" -TimeoutSec 3; break } catch {}
}
if (-not $features) { Fail 'REST API never came up'; exit 1 }
Pass 'REST API up'

foreach ($i in 1..15) {
    $features = Invoke-RestMethod -Uri "$api/api/features" -TimeoutSec 10
    if ($features.process.attached) { break }
    Start-Sleep -Seconds 1
}
if (-not $features.process.attached) { Fail 'Did not attach to RDR2'; exit 1 }
Pass "Attached to RDR2 (PID $($features.process.processId))"

# --- master script --------------------------------------------------------
Head 'Enabling master script (registers the shared symbols)'
$r = Toggle '}]}{][{}{}{}Main CodeBase}]}{][{}{}{}'
if ($r.error) { Fail "Main CodeBase: $($r.error)" } else { Pass "Main CodeBase enabled (active=$($r.active))" }
Start-Sleep -Seconds 3

$syms = try { Invoke-RestMethod -Uri "$api/api/debug/symbols" -TimeoutSec 15 } catch { $null }
if ($syms) {
    Info "$($syms.count) symbols registered"
    foreach ($n in @('_pPlayer', '_pPos', '_GodMode', '_pStaminaBase')) {
        $s = $syms.symbols | Where-Object { $_.name -eq $n.ToLower() }
        if ($s) { Info ("  {0,-16} = {1}" -f $n, $s.address) }
    }
}

# _pPlayer is filled in by the injected hook the next time the game runs that
# code. A zero here means the hook is installed but has not fired yet.
$pp = DebugRead '_pPlayer' 8
if ($pp -and $pp.readable) {
    Info "_pPlayer bytes: $($pp.bytes)"
    if ($pp.bytes -match '^(00 )+00$') {
        Fail '_pPlayer is still zero - the hook has not been hit yet. Move around in game.'
    } else {
        Pass '_pPlayer populated by the injected hook'
    }
}

# --- teleport script ------------------------------------------------------
Head 'Enabling Single Player Teleport (registers _pPos)'
$r = Toggle 'Single Player Teleport'
if ($r.error) { Fail "Single Player Teleport: $($r.error)" } else { Pass "enabled (active=$($r.active))" }
Start-Sleep -Seconds 3

$pos = DebugRead '_pPos' 8
if ($pos) { Info "_pPos bytes: $($pos.bytes)" }

# --- read coordinates -----------------------------------------------------
Head 'Player coordinates'
$c = Coords
$ok = 0
foreach ($x in $c) {
    if ($x.valueError) { Fail "$($x.description) -> $($x.valueError)" }
    elseif ($x.value -in @('??', '0')) { Fail "$($x.description) -> $($x.value)" }
    else { Pass "$($x.description) = $($x.value)"; $ok++ }
}

if ($ok -ne 3) {
    Write-Host ''
    Write-Host 'Coordinates not resolving yet.' -ForegroundColor Yellow
    Info 'The hooks only fill their pointers when the game executes that code.'
    Info 'Move your character, then re-run this script.'
    Info 'Trainer left running. Stop with: Get-Process rdr2_trainer | Stop-Process -Force'
    exit 0
}

# --- teleport -------------------------------------------------------------
Head 'Teleport test (moves the player +25 on X)'
$before = @{}
foreach ($x in $c) { $before[$x.description] = [double]$x.value }
Info ("before: X={0} Y={1} Z={2}" -f $before['Coord - X'], $before['Coord - Y'], $before['Coord - Z'])

$newX = $before['Coord - X'] + 25
$body = @{ description = 'Coord - X'; value = "$newX" } | ConvertTo-Json -Compress
$w = try {
    Invoke-RestMethod -Uri "$api/api/features/set" -Method Post `
        -ContentType 'application/json' -Body $body -TimeoutSec 20
} catch { @{ error = $_.Exception.Message } }

if ($w.error) { Fail "Write failed: $($w.error)" }
Start-Sleep -Seconds 1

$after = Coords
$movedX = ($after | Where-Object { $_.description -eq 'Coord - X' }).value
Info "after : X=$movedX"

if ([math]::Abs([double]$movedX - $newX) -lt 5) {
    Pass 'Teleport worked - the write landed in the game'
    Write-Host "`nRESULT: full chain verified (scan, alloc, assemble, inject, read, write)." -ForegroundColor Green
} else {
    Fail "X did not move as expected (wanted ~$newX, got $movedX)"
    Info 'The game may be overwriting the value each frame; that is normal for'
    Info 'some coordinate writes and does not mean the write failed.'
}

Write-Host ''
Info 'Trainer still running. Stop it cleanly so the game keeps its original bytes:'
Info '  Invoke-RestMethod http://127.0.0.1:42069/api/features/deactivate_all -Method Post'
Info '  Get-Process rdr2_trainer | Stop-Process -Force'
