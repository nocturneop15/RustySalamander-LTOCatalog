# Dot-source before building: . .\env.ps1
# Puts cargo and the MinGW-w64 toolchain (needed by the x86_64-pc-windows-gnu target and SQLite) on PATH.
$mingw = (Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs*\mingw64\bin" -Directory -ErrorAction SilentlyContinue | Select-Object -First 1).FullName
$env:Path = "$env:USERPROFILE\.cargo\bin;$mingw;$env:Path"
