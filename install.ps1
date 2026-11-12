# Install/upgrade a verified GitHub release without Rust or administrator access.
& {
    $ErrorActionPreference = 'Stop'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    if ($env:OS -ne 'Windows_NT') { throw 'Use install.sh on macOS or Linux.' }
    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') { throw 'The prebuilt Windows binary supports x64. Other architectures need a source build.' }
    $releaseUrl = 'https://github.com/zhanxw/zhandu/releases'
    $version = $env:ZHANDU_VERSION
    if (-not $version) {
        $latest = Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/latest"
        $uri = if ($latest.BaseResponse.ResponseUri) { $latest.BaseResponse.ResponseUri } else { $latest.BaseResponse.RequestMessage.RequestUri }
        $version = $uri.AbsolutePath.TrimEnd('/').Split('/')[-1]
    }
    if ($version -notmatch '^v\d+\.\d+\.\d+$') { throw 'Version must have the form v0.6.1.' }
    $folder = "zhandu-$version-x86_64-pc-windows-msvc"
    $archive = "$folder.zip"
    $work = Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
    $staged = $null
    New-Item -ItemType Directory -Path $work | Out-Null
    try {
        Write-Host "Downloading zhandu $version (Windows x64)..."
        $zip = Join-Path $work $archive
        Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/download/$version/$archive" -OutFile $zip
        $manifest = Join-Path $work 'SHA256SUMS'
        Invoke-WebRequest -UseBasicParsing -Uri "$releaseUrl/download/$version/SHA256SUMS" -OutFile $manifest
        $hashes = @(Get-Content $manifest | ForEach-Object {
            $parts = $_ -split '\s+'
            if ($parts.Length -eq 2 -and $parts[1] -ceq $archive) { $parts[0] }
        })
        if ($hashes.Count -ne 1 -or (Get-FileHash $zip -Algorithm SHA256).Hash -ine $hashes[0]) {
            throw 'Missing checksum or checksum mismatch; existing installation was not changed.'
        }
        Expand-Archive -LiteralPath $zip -DestinationPath $work
        $binary = Join-Path (Join-Path $work $folder) 'zhandu.exe'
        $actual = & $binary --version
        if ($LASTEXITCODE -ne 0 -or $actual -cne "zhandu $($version.Substring(1))") {
            throw 'Downloaded binary cannot run or has an unexpected version.'
        }
        $installDir = if ($env:ZHANDU_INSTALL_DIR) { $env:ZHANDU_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'zhandu\bin' }
        $installDir = [IO.Path]::GetFullPath($installDir)
        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $destination = Join-Path $installDir 'zhandu.exe'
        $staged = Join-Path $installDir ([guid]::NewGuid().ToString() + '.exe')
        Copy-Item -LiteralPath $binary -Destination $staged
        if (Test-Path -LiteralPath $destination) {
            [IO.File]::Replace($staged, $destination, $null)
        } else {
            [IO.File]::Move($staged, $destination)
        }
        $staged = $null
        # Custom directories do not modify PATH, which also permits isolated tests.
        if (-not $env:ZHANDU_INSTALL_DIR) {
            $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
            if ($installDir -notin ($userPath -split ';')) {
                [Environment]::SetEnvironmentVariable('Path', "$installDir;$userPath", 'User')
            }
            if ($installDir -notin ($env:Path -split ';')) { $env:Path = "$installDir;$env:Path" }
        }
        Write-Host "Installed $actual to $destination"
        Write-Host 'Run this installer again to upgrade. Close zhandu before upgrading.'
    } finally {
        if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged -Force }
        Remove-Item -LiteralPath $work -Recurse -Force
    }
}
