# Changelog

## 0.2.0-alpha (2026-11-11)

- **Portable like RustySalamander:** the catalog and the settings are kept in the plugin's folder (`plugins\ltocatalog`), or in the user's `%APPDATA%\RustySalamander\plugins\ltocatalog` once RustySalamander is in multi-user mode. Before, the catalog was always in `%APPDATA%\RustySalamander\ltocatalog`, and the settings went there when the plugin's folder was read-only. A catalog from 0.1.0-alpha isn't picked up: move `catalog.sqlite` into the plugin's folder, or choose it in *Library settings…*.
- **`ltocat --program <folder>`** uses the catalog and library settings of the plugin in that RustySalamander folder, wherever the program keeps them. Next to `rustysalamander.exe`, `ltocat` does this by itself. Without either (or `--catalog`), commands that need the catalog say so instead of using `%APPDATA%`.

## 0.1.0-alpha (2026-11-11)

- First version: LTFS tapes in an LTO library browsed from an offline catalog; the robot loads tapes on demand; missing tapes are asked for in a window drawn by the program; copying from and to tapes, folders, renames, deletes; importing, formatting, the mail slot, notes about where tapes are kept.
- `ltocat`, the command line: checks the hardware, inventory, import, get, put, format.
- Tested on a simulated library only; see "Not tested yet on real hardware" in the README.
