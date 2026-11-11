# Builds the release and puts what to install into target\release\dist:
#   dist\plugins\ltocatalog\ltocatalog.spl + lang\   (copy the plugins folder next to rustysalamander.exe)
#   dist\ltocat.exe                                    (the command line, anywhere)
#   dist\ltocatalog-<version>.zip                      (both, for the library machine)
# Usage: .\package.ps1
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
. .\env.ps1
cargo build --release -p sal-plugin-ltocatalog -p ltocat
if ($LASTEXITCODE -ne 0) { throw "build failed" }
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = "target\release\dist"
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
$plugin = "$dist\plugins\ltocatalog"
New-Item -ItemType Directory -Force "$plugin\lang" | Out-Null
Copy-Item target\release\sal_plugin_ltocatalog.dll "$plugin\ltocatalog.spl"
Copy-Item plugin\lang\*.toml "$plugin\lang\"
Copy-Item target\release\ltocat.exe $dist
Copy-Item README.md, CHANGELOG.md $dist
Compress-Archive -Path "$dist\plugins", "$dist\ltocat.exe", "$dist\README.md", "$dist\CHANGELOG.md" -DestinationPath "$dist\ltocatalog-$version.zip"
Get-ChildItem -Recurse $dist | Where-Object { -not $_.PSIsContainer } | ForEach-Object { "{0,10:N0}  {1}" -f $_.Length, $_.FullName.Substring((Resolve-Path $dist).Path.Length + 1) }
