# Install readout — usage statistics for Claude Code and Codex.
#
#   powershell -c "irm https://raw.githubusercontent.com/atoz03/readout-cli/main/install.ps1 | iex"
#
# Downloads the release binary for this platform, verifies its published
# SHA-256, and installs it. Nothing else on the system is touched — PATH
# included: when the install directory is not on it, this prints the command
# rather than editing your environment behind your back.
#
#   $env:READOUT_VERSION = 'v0.1.0'      pin a version instead of taking the latest
#   $env:READOUT_INSTALL_DIR = 'C:\bin'  install somewhere other than the default
#
# The body is one script block on purpose: piped into `iex` it runs in the
# caller's own session, and a bare assignment would leave their
# $ErrorActionPreference and $ProgressPreference changed after we exit.
& {
  $ErrorActionPreference = 'Stop'
  # Invoke-WebRequest repaints a progress bar per chunk on Windows PowerShell,
  # which costs several times the download itself on a slow console.
  $ProgressPreference = 'SilentlyContinue'

  $repo = 'atoz03/readout-cli'

  function say($msg) { Write-Host $msg }
  # `throw`, not `exit`: under `irm | iex` this is the caller's session, and
  # exiting it would close their terminal over a failed download.
  function die($msg) { throw "install: $msg" }

  if ($PSVersionTable.PSVersion.Major -lt 5) {
    die 'Windows PowerShell 5.1 or newer is required (for Expand-Archive and Get-FileHash).'
  }
  # $IsWindows only exists on PowerShell 6+; on 5.1 it is $null and the check
  # is moot, because 5.1 is Windows-only.
  if ($PSVersionTable.PSVersion.Major -ge 6 -and -not $IsWindows) {
    die 'this is the Windows installer. On Linux and macOS use install.sh.'
  }

  # Windows PowerShell 5.1 still negotiates TLS 1.0 by default on older builds,
  # which GitHub refuses outright. Opting in costs nothing where 1.2 is already
  # the default.
  [Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  # After the platform check, not before: %LOCALAPPDATA% is a Windows variable,
  # and resolving it first turns "you want install.sh" into a null-argument
  # error from Join-Path on the very systems that need to be told.
  if ($env:READOUT_INSTALL_DIR) {
    $installDir = $env:READOUT_INSTALL_DIR
  } elseif ($env:LOCALAPPDATA) {
    $installDir = Join-Path $env:LOCALAPPDATA 'Programs\readout'
  } else {
    die 'LOCALAPPDATA is not set; point READOUT_INSTALL_DIR at a directory to install into.'
  }

  # A 32-bit PowerShell on 64-bit Windows reports x86 and puts the real
  # architecture in PROCESSOR_ARCHITEW6432.
  $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 }
          else { $env:PROCESSOR_ARCHITECTURE }
  $note = ''
  switch ($arch) {
    'AMD64' { }
    # There is no arm64 Windows build yet. The x64 one runs under emulation on
    # ARM devices, so install it and say so rather than refusing.
    # ASCII in the messages, deliberately: PowerShell 5.1 reads a .ps1 with no
    # BOM as ANSI, so an em dash in a string prints as mojibake for anyone who
    # downloads this file and runs it directly. A BOM would fix that and break
    # `irm | iex`, which is the documented path.
    'ARM64' { $note = '  no arm64 build yet - installing the x64 binary, which Windows emulates' }
    default { die "unsupported architecture: $arch. Try 'cargo install readout' to build from source." }
  }
  $target = 'x86_64-pc-windows-msvc'

  $version = $env:READOUT_VERSION
  if (-not $version) {
    try {
      $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest" -UseBasicParsing).tag_name
    } catch {
      die 'could not reach the GitHub API; set READOUT_VERSION to pin a release'
    }
    if (-not $version) { die 'could not determine the latest release; set READOUT_VERSION to pin one' }
  }

  $archive = "readout-$version-$target.zip"
  $base = "https://github.com/$repo/releases/download/$version"
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ('readout-' + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp | Out-Null

  try {
    say "readout $version ($target)"
    if ($note) { say $note }

    $zip = Join-Path $tmp $archive
    try {
      Invoke-WebRequest "$base/$archive" -OutFile $zip -UseBasicParsing
    } catch {
      die "no build for $target in $version"
    }

    # Verify against the checksums published with the release. A silent
    # mismatch is the one failure mode worth spending a second on.
    $sums = Join-Path $tmp 'SHA256SUMS'
    try {
      Invoke-WebRequest "$base/SHA256SUMS" -OutFile $sums -UseBasicParsing
    } catch {
      die "could not download SHA256SUMS for $version"
    }
    # sha256sum wrote the zip in binary mode, so its line reads `<hash> *<name>`
    # while the tarballs read `<hash>  <name>`. Tolerate both markers.
    $expected = Get-Content $sums | ForEach-Object {
      if ($_ -match '^([0-9a-fA-F]{64})\s+\*?(.+)$' -and $matches[2] -eq $archive) { $matches[1] }
    } | Select-Object -First 1
    if (-not $expected) { die "SHA256SUMS has no entry for $archive" }
    # Get-FileHash returns uppercase hex and sha256sum writes lowercase.
    # PowerShell's -ne on strings is case-insensitive, which is what we want
    # here and the reason this is not comparing normalized copies.
    $actual = (Get-FileHash $zip -Algorithm SHA256).Hash
    if ($actual -ne $expected) {
      die "checksum mismatch for ${archive}: expected $expected, got $actual"
    }
    say '  checksum ok'

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $binary = Join-Path $tmp "readout-$version-$target\readout.exe"
    if (-not (Test-Path $binary)) { die 'the archive did not contain a readout.exe' }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $dest = Join-Path $installDir 'readout.exe'
    try {
      Move-Item -Path $binary -Destination $dest -Force
    } catch {
      # Windows locks a running image, unlike the Unix rename install.sh does.
      die "could not replace ${dest}: close any running readout and try again."
    }
    # We verified this binary against the checksum the release publishes, so the
    # download mark only costs the user a SmartScreen prompt on first run.
    # Wrapped rather than -ErrorAction SilentlyContinue, which does not cover a
    # terminating error: the install is already done by this line, and nothing
    # this cmdlet can fail at is worth reporting as a failed install.
    try { Unblock-File -Path $dest } catch { }
    say "  installed $dest"
  } finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }

  # Both separators, because Windows accepts either and a PATH entry written
  # with a trailing one is the same directory. Comparison is -contains, which
  # is case-insensitive on strings — as Windows paths are.
  $onPath = ($env:PATH -split ';' | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\', '/') }) -contains $installDir.TrimEnd('\', '/')
  say ''
  if ($onPath) {
    say "Run 'readout' to open the dashboard."
  } else {
    say "$installDir is not on your PATH. Add it for this session:"
    say "  `$env:PATH = '$installDir;' + `$env:PATH"
    # Deliberately not `setx PATH "$env:PATH;..."`, which is the usual advice
    # and is wrong twice: it truncates at 1024 characters, and $env:PATH is the
    # machine and user paths already merged, so writing it back stamps the whole
    # merged value into the user's own PATH. Reading the User scope and
    # appending to that is the form that cannot eat someone's environment.
    say 'or for good, in this session:'
    say "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ';$installDir', 'User')"
    say "Then run 'readout' to open the dashboard."
  }
}
