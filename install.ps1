# Install frust on Windows.
#   irm https://raw.githubusercontent.com/Seggys116/frust/main/install.ps1 | iex
# Environment: FRUST_VERSION (for example v0.1.0, default latest), FRUST_INSTALL_DIR (default %LOCALAPPDATA%\frust\bin).
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'Seggys116/frust'
$bins = 'frust', 'fcargo', 'fclippy', 'frustc', 'frustdoc', 'frustup', 'frustfmt', 'frust-wrap'
$version = if ($env:FRUST_VERSION) { $env:FRUST_VERSION } else { 'latest' }
$dest = if ($env:FRUST_INSTALL_DIR) { $env:FRUST_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'frust\bin' }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'aarch64' }
    default { throw "unsupported architecture $($env:PROCESSOR_ARCHITECTURE)" }
}
$target = "$arch-pc-windows-msvc"
$archive = "frust-$target.zip"
$base = if ($version -eq 'latest') {
    "https://github.com/$repo/releases/latest/download"
} else {
    "https://github.com/$repo/releases/download/$version"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("frust-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "frust-install: downloading $archive ($version)"
    Invoke-WebRequest "$base/$archive" -OutFile (Join-Path $tmp $archive)
    Invoke-WebRequest "$base/$archive.sha256" -OutFile (Join-Path $tmp "$archive.sha256")

    $expected = ((Get-Content (Join-Path $tmp "$archive.sha256") -Raw).Trim() -split '\s+')[0].ToLower()
    $actual = (Get-FileHash (Join-Path $tmp $archive) -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { throw "checksum mismatch (expected $expected, got $actual)" }

    Expand-Archive (Join-Path $tmp $archive) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    foreach ($b in $bins) {
        $src = Join-Path $tmp "$b.exe"
        if (-not (Test-Path $src)) { throw "archive is missing $b.exe" }
        Copy-Item $src (Join-Path $dest "$b.exe") -Force
    }
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "frust-install: installed to $dest"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $dest) {
    [Environment]::SetEnvironmentVariable('Path', "$dest;$userPath", 'User')
    Write-Host 'frust-install: added to your user PATH; open a new terminal to use it'
}
Write-Host "frust-install: run 'frust doctor' to check your environment"
