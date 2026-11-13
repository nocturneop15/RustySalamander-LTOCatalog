# Changelog

## 0.3.1-alpha (2026-11-13)

- **A disk is never used as a tape drive or a changer:** a device named in *Library settings…* (or found) gets tape commands only if it says it is a tape drive, and changer commands only if it says it is a medium changer. Before, `\\.\PhysicalDrive0` typed as a drive would have received FORMAT on *Format a tape*. A device that isn't one is left out, with a note in *Library status*.
- **The catalog is never a device:** *Library settings…* and `ltocat --catalog` take only a whole path to a file in a folder that exists; `NUL`, `\\.\…`, `CON.sqlite` and folders are refused with the reason.
- **Typed input checked:** a barcode is letters A–Z and digits (at most 16); a volume name has no control characters (they would make the tape's index invalid XML for other LTFS software); a note is one line; device names have no control characters. Every form was tried with RustySalamander's list of hostile input.
- Built against RustySalamander 0.9.7-beta's plugin interface: a name that looks like a placeholder (`{error}`) shows as it is in messages.

## 0.3.0-alpha (2026-11-11)

- **Standalone drives:** drives outside any library are used for tapes put in by hand. The tape in one is recognised by the barcode in its cartridge memory, else by its LTFS volume (its UUID in the catalog, or the volume serial of its VOL1 label), once per cartridge. A missing tape is asked for with where it can go: a library's mail slot or a standalone drive, and what each drive holds. When every standalone drive holds another tape, the least recently used one is ejected first. The eject button is locked while a job uses the drive. A new tape in a standalone drive can be formatted; it gets its barcode in the cartridge memory.
- **Several libraries:** every changer found is used, each with the drives that report to it; the catalog says which library a tape is in, and the same barcode in two libraries is noted. *Library settings…* and `ltocat --changer` take several changers, comma-separated.
- **Libraries and drives connected later** are found by *Inventory the libraries and drives* and *Library status*.
- **Eject a tape…** (was *Move a tape to the mail slot…*): a standalone drive ejects the tape.
- **`ltocat`:** `selftest`, `inventory`, `load`, `eject`, `insert` work on every library and standalone drive; the simulator has `--sim-libraries`, `--sim-drives` and `sim-move` to move tapes by hand. A question whose input has ended (a redirected stdin) cancels instead of retrying forever.

## 0.2.0-alpha (2026-11-11)

- **Portable like RustySalamander:** the catalog and the settings are kept in the plugin's folder (`plugins\ltocatalog`), or in the user's `%APPDATA%\RustySalamander\plugins\ltocatalog` once RustySalamander is in multi-user mode. Before, the catalog was always in `%APPDATA%\RustySalamander\ltocatalog`, and the settings went there when the plugin's folder was read-only. A catalog from 0.1.0-alpha isn't picked up: move `catalog.sqlite` into the plugin's folder, or choose it in *Library settings…*.
- **`ltocat --program <folder>`** uses the catalog and library settings of the plugin in that RustySalamander folder, wherever the program keeps them. Next to `rustysalamander.exe`, `ltocat` does this by itself. Without either (or `--catalog`), commands that need the catalog say so instead of using `%APPDATA%`.

## 0.1.0-alpha (2026-11-11)

- First version: LTFS tapes in an LTO library browsed from an offline catalog; the robot loads tapes on demand; missing tapes are asked for in a window drawn by the program; copying from and to tapes, folders, renames, deletes; importing, formatting, the mail slot, notes about where tapes are kept.
- `ltocat`, the command line: checks the hardware, inventory, import, get, put, format.
- Tested on a simulated library only; see "Not tested yet on real hardware" in the README.
