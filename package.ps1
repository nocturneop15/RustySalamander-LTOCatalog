# Builds the release and puts what to install into target\release\dist:
#   dist\plugins\ltocatalog\ltocatalog.spl + lang\ + ltocat.exe + README.md, CHANGELOG.md, SIMULATOR.md
#                                                      (copy the plugins folder next to rustysalamander.exe;
#                                                       ltocat and the documents are in the plugin's folder,
#                                                       so the program's plugin updates bring them too)
#   dist\ltocatalog-<version>.zip                      (that, for the library machine)
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
Copy-Item target\release\ltocat.exe $plugin
# the documents in the plugin's folder: updated with it, and the program shows its CHANGELOG
Copy-Item README.md, CHANGELOG.md, SIMULATOR.md $plugin
Compress-Archive -Path "$dist\plugins" -DestinationPath "$dist\ltocatalog-$version.zip"
# the plugin folder alone, for RustySalamander's update check (Plugin::UPDATES): <id>-<version>-api<interface>.zip
$api = (Select-String -Path ..\RustySalamander\crates\sal-plugin-api\src\abi.rs -Pattern 'pub const VERSION: u32 = (\d+)').Matches[0].Groups[1].Value
Compress-Archive -Path $plugin -DestinationPath "$dist\ltocatalog-$version-api$api.zip"
Get-ChildItem -Recurse $dist | Where-Object { -not $_.PSIsContainer } | ForEach-Object { "{0,10:N0}  {1}" -f $_.Length, $_.FullName.Substring((Resolve-Path $dist).Path.Length + 1) }
