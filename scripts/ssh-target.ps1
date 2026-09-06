[CmdletBinding()]
param(
    [ValidateSet('up', 'stop', 'down', 'destroy', 'reset-origin', 'status', 'config')]
    [string]$Action = 'up',
    [ValidateRange(1, 65535)]
    [int]$Port = 2222
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$stateDir = Join-Path $repoRoot '.docker-ssh-target'
$privateKey = Join-Path $stateDir 'id_ed25519'
$publicKey = "$privateKey.pub"
$authorizedKeys = Join-Path $stateDir 'authorized_keys'
$knownHosts = Join-Path $stateDir 'known_hosts'
$sshConfig = Join-Path $stateDir 'ssh_config'
$composeFile = Join-Path $repoRoot 'docker\ssh-target-compose.yml'

function Set-FixtureEnvironment {
    $env:RALPHUS_SSH_TEST_PORT = $Port.ToString()
    $env:RALPHUS_SSH_TEST_AUTHORIZED_KEYS = $authorizedKeys
}

function Ensure-TestKey {
    New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
    if (-not (Test-Path -LiteralPath $privateKey)) {
        & ssh-keygen -q -t ed25519 -N '' -C 'ralphus-docker-test' -f $privateKey
        if ($LASTEXITCODE -ne 0) {
            throw "ssh-keygen failed with exit code $LASTEXITCODE"
        }
    }
    Copy-Item -LiteralPath $publicKey -Destination $authorizedKeys -Force
}

function Write-SshConfig {
    $identity = $privateKey.Replace('\', '/')
    $known = $knownHosts.Replace('\', '/')
    $content = @"
Host ralphus-docker
    HostName 127.0.0.1
    Port $Port
    User ralphus
    IdentityFile $identity
    IdentitiesOnly yes
    UserKnownHostsFile $known
    StrictHostKeyChecking yes
    BatchMode yes
"@
    Set-Content -LiteralPath $sshConfig -Value $content -Encoding ascii
}

function Invoke-Compose {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)
    Set-FixtureEnvironment
    & docker compose --file $composeFile @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "docker compose failed with exit code $LASTEXITCODE"
    }
}

switch ($Action) {
    'up' {
        Ensure-TestKey
        Set-FixtureEnvironment
        Invoke-Compose up --build --detach --wait
        $hostPublicKey = Invoke-Compose exec --no-tty target cat /etc/ssh/ralphus-host-keys/ssh_host_ed25519_key.pub
        $hostKeyParts = ($hostPublicKey | Out-String).Trim().Split(' ', [System.StringSplitOptions]::RemoveEmptyEntries)
        if ($hostKeyParts.Count -lt 2) {
            throw 'could not read the fixture host public key'
        }
        $knownHostLine = "[127.0.0.1]:$Port $($hostKeyParts[0]) $($hostKeyParts[1])"
        Set-Content -LiteralPath $knownHosts -Value $knownHostLine -Encoding ascii
        Write-SshConfig
        Write-Host 'Ralphus Docker SSH target is ready.'
        Write-Host "SSH: ssh -F `"$sshConfig`" ralphus-docker"
        Write-Host "Provider target: ralphus-docker"
        Write-Host "Provider environment: RALPHUS_SSH_CONFIG_FILE=$sshConfig"
        $provider = Join-Path $repoRoot 'target\debug\ralphus-ssh-provider.exe'
        Write-Host 'Build provider: cargo build --package ralphus-ssh-provider'
        Write-Host "Register provider: ralphus machine register --scheme ssh --program `"$provider`" --arg=--ssh-config --arg `"$sshConfig`" --description `"Docker SSH target fixture`""
        Write-Host 'Task machine value: ssh:ralphus-docker'
        Write-Host 'Fixture Git URL: file:///srv/git/ralphus-test.git'
        Write-Host 'Remote root: /home/ralphus/.ralphus/remote-work'
    }
    'stop' { Invoke-Compose stop }
    'down' { Invoke-Compose down }
    'destroy' {
        Invoke-Compose down --volumes --remove-orphans
        Write-Host "Docker volumes were removed. Generated SSH material remains in $stateDir."
    }
    'reset-origin' {
        # RAL-355: reseeds only the bare Git origin to its pristine state --
        # unlike 'destroy', this leaves the remote root and pinned host key
        # untouched, so a test suite can get a clean origin between runs
        # without forcing a full re-'up' (and re-pin) afterward.
        Invoke-Compose exec --no-tty target /usr/local/bin/reset-origin.sh --force
        Write-Host 'Fixture Git origin reset to its pristine seeded state.'
    }
    'status' { Invoke-Compose ps }
    'config' {
        Ensure-TestKey
        Write-SshConfig
        Write-Host $sshConfig
    }
}
